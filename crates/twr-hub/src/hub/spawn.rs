//! Retry-safe, durable thread creation. A committed thread and its first prompt are atomic.

use super::{internal, rpc, Caller, Hub, NewThread};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use twr_core::proto::{codes, RpcError};

impl Hub {
    pub fn begin_spawn(
        &self,
        operation_id: &str,
        mut request: NewThread,
        prompt: Option<String>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if operation_id.is_empty() || operation_id.len() > 128 {
            return Err(rpc(codes::BAD_REQUEST, "operation_id must be 1–128 bytes"));
        }
        if prompt
            .as_ref()
            .is_some_and(|p| p.trim().is_empty() || p.len() > 256 * 1024)
        {
            return Err(rpc(
                codes::BAD_REQUEST,
                "prompt must be nonempty and at most 256 KiB",
            ));
        }
        // Keep a fixed-size request digest for permanent deduplication, rather than
        // retaining every first prompt in this table alongside the event log.
        let encoded = twr_core::blob::blob_ref(
            &serde_json::to_vec(&json!({"request": request, "prompt": prompt}))
                .map_err(internal)?,
        );
        let owner = if caller.is_orchestrator() {
            "orchestrator"
        } else {
            "user"
        };
        {
            let core = self.inner.core();
            let conn = core.store.conn();
            let previous: Option<(String, String)> = conn
                .query_row(
                    "SELECT owner, request FROM spawn_operations WHERE operation_id=?1",
                    [operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(internal)?;
            if let Some((who, body)) = previous {
                if who != owner || body != encoded {
                    return Err(rpc(
                        codes::CONFLICT,
                        "operation_id already belongs to a different spawn request",
                    ));
                }
                drop(core);
                return self.spawn_status(operation_id, caller);
            }
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM spawn_operations WHERE result IS NULL",
                    [],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            if pending >= 64 {
                return Err(rpc(
                    codes::CONFLICT,
                    "spawn queue is full; retry later with this operation_id",
                ));
            }
            conn.execute("INSERT INTO spawn_operations(operation_id,owner,request,created) VALUES (?1,?2,?3,?4)",
                params![operation_id, owner, encoded, chrono::Utc::now().to_rfc3339()]).map_err(internal)?;
            core.store.sync().map_err(internal)?;
        }
        request.spawn_operation = Some(operation_id.into());
        request.first_prompt = prompt;
        let hub = self.clone();
        let id = operation_id.to_string();
        let task_caller = caller.clone();
        tokio::spawn(async move {
            let _slot = hub.inner.spawn_slots.clone().acquire_owned().await;
            let result = match hub.new_thread_by(request, &task_caller).await {
                Ok(thread) => json!({"status":"complete", "thread":thread}),
                Err(error) => json!({"status":"error", "error":error}),
            };
            if let Err(e) = hub.finish_spawn(&id, &result) {
                tracing::error!(operation_id=%id, error=%e, "recording spawn result failed");
            }
        });
        self.spawn_status(operation_id, caller)
    }

    pub fn spawn_status(&self, id: &str, caller: &Caller) -> Result<Value, RpcError> {
        // The orchestrator learns how loaded the machine is as it starts work on it.
        let load = caller
            .is_orchestrator()
            .then(twr_core::load::Load::sample_cached)
            .flatten();
        let core = self.inner.core();
        let row: Option<(String, Option<String>)> = core
            .store
            .conn()
            .query_row(
                "SELECT owner,result FROM spawn_operations WHERE operation_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((owner, result)) = row else {
            return Err(rpc(codes::NOT_FOUND, "unknown spawn operation"));
        };
        if caller.is_orchestrator() && owner != "orchestrator" {
            return Err(rpc(codes::CONFLICT, "spawn operation belongs to the user"));
        }
        let mut out = match result {
            Some(r) => serde_json::from_str::<Value>(&r).map_err(internal)?,
            None => json!({"status":"pending"}),
        };
        if out["status"] == "complete" {
            let id = out["thread"]["id"].as_str().map(str::to_string);
            if let Some(thread) = id.as_deref().and_then(|t| core.registry.get(t)) {
                if let Some(note) = machine_load_note(load.as_ref(), &thread.box_name) {
                    out["machine_load"] = json!(note);
                }
                out["thread"] = core.thread_json(thread);
                for key in ["send", "project_found"] {
                    if let Some(value) = out.get(key).cloned() {
                        out["thread"][key] = value;
                    }
                }
            } else if let Some((id, view)) =
                id.and_then(|t| core.registry.deleted_view(&t).map(|v| (t, v)))
            {
                // The thread the operation made was deleted since (it may be in the trash, and
                // its name may belong to another thread now): the operation stays complete,
                // and says so, so a retry never makes a second thread for it.
                out["thread"] = json!({"id": id, "name": view.name, "deleted": true});
            }
        }
        out["operation_id"] = json!(id);
        Ok(out)
    }

    fn finish_spawn(&self, id: &str, result: &Value) -> Result<(), RpcError> {
        let core = self.inner.core();
        let stored = if result["status"] == "complete" {
            let mut compact = json!({"status":"complete", "thread":{"id":result["thread"]["id"]}});
            for key in ["send", "project_found"] {
                if let Some(value) = result["thread"].get(key) {
                    compact[key] = value.clone();
                }
            }
            compact
        } else {
            result.clone()
        };
        core.store
            .conn()
            .execute(
                "UPDATE spawn_operations SET result=?2 WHERE operation_id=?1 AND result IS NULL",
                params![id, serde_json::to_string(&stored).map_err(internal)?],
            )
            .map_err(internal)?;
        core.store.sync().map_err(internal)
    }

    /// Never repeat git creation after a restart. Recover committed threads, fail unfinished
    /// operations explicitly (a workspace prepared before the commit may need inspection).
    pub(super) fn recover_spawns(&self) -> anyhow::Result<()> {
        let core = self.inner.core();
        let conn = core.store.conn();
        let mut stmt =
            conn.prepare("SELECT operation_id FROM spawn_operations WHERE result IS NULL")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let created: Option<(String, Option<String>)> = conn.query_row(
                "SELECT thread, json_extract(data,'$.ext.twr.project_found') FROM events WHERE kind='thread.created' AND json_extract(data,'$.ext.twr.spawn_operation')=?1 LIMIT 1",
                [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            // Committed: complete, also when the thread was deleted since (its id stays in
            // the log; `spawn_status` says it was deleted).
            let thread = created.as_ref().map(|(t, _)| t.clone());
            let result = match thread {
                Some(id) => {
                    let mut out =
                        json!({"status":"complete", "thread":{"id":id}, "recovered":true});
                    if let Some(found) = created.as_ref().and_then(|(_, f)| f.as_ref()) {
                        out["project_found"] = serde_json::from_str(found)?;
                    }
                    out
                }
                None => json!({"status":"error", "error": RpcError::new(codes::CONFLICT,
                    "hub restarted before thread creation committed; inspect any leftover worktree before starting a new operation")}),
            };
            conn.execute(
                "UPDATE spawn_operations SET result=?2 WHERE operation_id=?1",
                params![id, serde_json::to_string(&result)?],
            )?;
        }
        core.store.sync()?;
        Ok(())
    }
}

/// What a spawn result tells the orchestrator about the machine its thread runs on: the hub
/// machine's load ([`twr_core::load::Load::summary`]) while it is busy or saturated and the
/// thread is local. A thread on another box runs on that box's load, which the hub doesn't read.
fn machine_load_note(load: Option<&twr_core::load::Load>, box_name: &str) -> Option<String> {
    load.filter(|l| box_name == "local" && l.level() >= twr_core::load::Level::Busy)
        .map(twr_core::load::Load::summary)
}

#[cfg(test)]
mod load_tests {
    use super::machine_load_note;
    use twr_core::load::Load;

    #[test]
    fn spawn_results_name_a_busy_local_machine() {
        let at = |load1: &str| Load::from_proc(None, None, None, Some(load1), 4);
        let saturated = at("13 1 1 1/1 1");
        assert_eq!(
            machine_load_note(saturated.as_ref(), "local").as_deref(),
            Some("saturated: load 13.0 on 4 cores")
        );
        assert!(machine_load_note(at("6 1 1 1/1 1").as_ref(), "local")
            .unwrap()
            .starts_with("busy: "));
        assert_eq!(machine_load_note(at("1 1 1 1/1 1").as_ref(), "local"), None);
        assert_eq!(machine_load_note(saturated.as_ref(), "gpu"), None);
        assert_eq!(machine_load_note(None, "local"), None);
    }
}

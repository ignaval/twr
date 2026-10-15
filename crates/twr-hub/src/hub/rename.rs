//! Renaming a worker thread: `thread.rename` (`twr rename`, the TUI's `/rename`, the
//! orchestrator's `rename_thread`), recorded as `thread.renamed`.
//!
//! The new name follows the rules of a new thread's (`threads::validate_thread_name`) and must
//! be free: no thread's id, name, alias or former name, and not reserved by a thread being
//! created, in any case (`api` and `API` are one name). A thread may take back one of its own
//! former names, or change the case of its name.
//!
//! Only the name changes. The worktree path and the branch keep the name the thread had when
//! they were made (`<worktrees>/<project>/<old name>`, `twr/<old name>`, as `thread.created`
//! recorded them): moving a worktree under a running agent would pull its working directory
//! away, Claude Code and Codex keep their sessions by directory (a moved worktree would no
//! longer resume them), the branch may be pushed or checked out elsewhere, and a remote box
//! would need a git operation of its own. So the earlier names stay the thread's
//! (`ThreadView::former_names`): no other thread takes them (its worktree and branch would
//! collide with these), and they still reach it (`Registry::resolve`), so a reference written
//! before the rename (a notice, the orchestrator's `<thread name="...">` boxes) finds it. The
//! aliases stay too.
//!
//! The orchestrator's call is applied directly (a name is no permission), recorded with `by:
//! orchestrator`. Orchestrator threads are never renamed: `orchestrator` / `orch` are how the
//! user and the hub find the current one.

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::{append_err, git, rpc, threads, Caller, Core, Hub, Inner};

impl Hub {
    /// `thread.rename {thread, name}`: gives a worker thread a new name (see the module docs).
    /// Refused while the hub shuts down or the thread is being deleted (`conflict`), for an
    /// orchestrator thread (`bad_request`, `conflict` for a retired one), an invalid name
    /// (`bad_request`) and a name that is taken (`conflict`).
    ///
    /// Returns `{thread, thread_name, previous, changed, worktree?, branch?, message}`.
    pub fn rename_thread(
        &self,
        query: &str,
        name: &str,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        core.rename_locked(&self.inner, query, name, caller)
    }
}

impl Core {
    /// [`Hub::rename_thread`] under the core lock.
    fn rename_locked(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        name: &str,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let id = self.resolve(query)?;
        self.refuse_retired(&id)?;
        self.refuse_deleting(&id)?;
        let view = self.view(&id)?.clone();
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is the orchestrator: it keeps its name (only worker threads are renamed)",
                    view.name
                ),
            ));
        }
        threads::validate_thread_name(name)?;
        let branch = match self.rt.get(&id).map(|r| &r.workspace) {
            Some(git::Workspace::Worktree { branch, .. }) => Some(branch.clone()),
            _ => None,
        };
        let mut out = json!({
            "thread": id,
            "thread_name": name,
            "previous": view.name,
            "worktree": view.worktree,
            "branch": branch,
        });
        if name == view.name {
            out["changed"] = json!(false);
            out["message"] = json!(format!("{name} already has that name"));
            return Ok(out);
        }
        if view.aliases.iter().any(|a| super::same_name(a, name)) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name:?} is an alias of {} already", view.name),
            ));
        }
        if self.name_taken_but(name, Some(&id)) {
            return Err(rpc(codes::CONFLICT, format!("name {name:?} is taken")));
        }
        let by = if caller.is_orchestrator() {
            ChangedBy::Orchestrator
        } else {
            ChangedBy::User
        };
        self.append(
            hub,
            &id,
            None,
            caller.source(),
            EventBody::ThreadRenamed(ThreadRenamed {
                name: name.to_string(),
                previous: view.name.clone(),
                by,
                ..Default::default()
            }),
        )
        .map_err(append_err)?;
        out["changed"] = json!(true);
        let mut message = format!("renamed {} to {name}", view.name);
        match (&view.worktree, &branch) {
            (Some(w), Some(b)) => message.push_str(&format!(
                "; its worktree {w} and branch {b} keep their names"
            )),
            (_, Some(b)) => message.push_str(&format!("; its branch {b} keeps its name")),
            _ => {}
        }
        out["message"] = json!(message);
        Ok(out)
    }
}

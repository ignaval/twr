//! The boxes' agent CLIs as the hub knows them (`hub.info` `agents`, `box.agents`), the
//! warning a thread gets on a box that reports its agent's CLI missing, and how much of the
//! boxes each caller sees.
//!
//! **Remote boxes** report theirs (`twr_core::runner::AgentInfo`): the inventory of each
//! connection (possibly none: a runner sends what its first probe found, which may not have
//! ended), then the answers to `Op::Agents` the hub asks on that connection [`AGENTS_FIRST`]
//! after its welcome and every [`AGENTS_EVERY`] ([`poll`]: `refresh: false`, the runner's
//! last probe), and on demand (`box.agents` with `refresh`: the runner probes again first, for
//! a CLI just installed or signed in). An answer is kept only while the connection it was
//! asked on is still the box's current one, and a box's requests go one at a time, each
//! answer kept before the next is asked, so an older answer never replaces a newer one
//! ([`RemoteBox::refresh_agents`]).
//!
//! **The hub's own box** (`local`): the hub probes its agents itself
//! ([`HubOptions::agent_probe`](crate::HubOptions::agent_probe): `twr_adapters::agents` with
//! the hub's own environment, no login shell asked, as the hub's own runtimes get none), in
//! the background once it is open and every [`AGENTS_EVERY`]: it never delays the start.
//! Tests probe nothing, or a fake.
//!
//! **Advisory only.** What a box reports refuses nothing: a thread on a built-in agent runtime
//! whose CLI the box's last report says is not found is still created (`thread.new`,
//! `thread.spawn`) or moved there (`thread.set_model`), and the result carries a `warning`
//! ([`agent_warning`]). The probe sees the box's baseline environment, and a thread may
//! configure a `PATH` of its own.
//!
//! **Who sees what** ([`BoxView`]): a user everything; the orchestrator and a thread's agent
//! per box only `name, state, online, version, agents`, and per agent only `runtime, found,
//! login` ([`project_box`], [`project_agents`]: no paths of the box, no `--version` line, sign-in
//! method or note, no notes or errors the box's relay printed, no OS, CPU or builds). The same
//! projection shapes `hub.info` and `box.agents`. Nothing a probe printed is logged here.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use serde_json::{json, Map, Value};
use tokio::sync::watch;
use twr_core::proto::{codes, RpcError};
use twr_core::runner::AgentInfo;

use super::{rpc, Hub, HubState, Inner, LOCAL_BOX};
use crate::remote::RemoteBox;

/// How long after a box's welcome the hub first asks it for its agents.
pub const AGENTS_FIRST: Duration = Duration::from_secs(30);

/// How often the hub asks a box for its agents again, and probes its own.
pub const AGENTS_EVERY: Duration = Duration::from_secs(10 * 60);

/// How long a box's answer to `Op::Agents` is waited for.
pub const AGENTS_TIMEOUT: Duration = Duration::from_secs(30);

/// How the hub probes its own box's agent CLIs ([`crate::HubOptions::agent_probe`]).
pub type AgentProbe = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentInfo>> + Send + Sync>;

/// The hub's own probe: `twr_adapters::agents::probe` with the hub's environment (no login
/// shell: the hub's runtimes start with its own `PATH`). What `twr hub` uses.
pub fn own_agent_probe() -> AgentProbe {
    Arc::new(|| twr_adapters::agents::probe(None).boxed())
}

/// When the hub asks the boxes for their agents ([`crate::HubOptions::agents_poll`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentsPoll {
    /// After a box's welcome ([`AGENTS_FIRST`]).
    pub first: Duration,
    /// Then this often, and the hub's own probe too ([`AGENTS_EVERY`]).
    pub every: Duration,
}

impl Default for AgentsPoll {
    fn default() -> Self {
        AgentsPoll {
            first: AGENTS_FIRST,
            every: AGENTS_EVERY,
        }
    }
}

/// Who reads the boxes, and so how much of them ([`project_box`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxView {
    /// A user: everything.
    Full,
    /// The orchestrator or a thread's agent: a box's state and which agents it has, signed
    /// in or not.
    Limited,
}

/// The fields of a box ([`BoxView::Limited`]).
const LIMITED_BOX: &[&str] = &["name", "state", "online", "version", "agents"];

/// The fields of an agent ([`BoxView::Limited`]).
const LIMITED_AGENT: &[&str] = &["runtime", "found", "login"];

/// Box entry `b` of `hub.info` as `view` sees it (see the module docs).
pub fn project_box(b: Value, view: BoxView) -> Value {
    let Value::Object(mut b) = b else { return b };
    if view == BoxView::Limited {
        b.retain(|k, _| LIMITED_BOX.contains(&k.as_str()));
    }
    if let Some(a) = b.remove("agents") {
        b.insert("agents".into(), project_agents(a, view));
    }
    Value::Object(b)
}

/// A box's agents (a JSON array of `AgentInfo`) as `view` sees them.
pub fn project_agents(agents: Value, view: BoxView) -> Value {
    match (agents, view) {
        (a, BoxView::Full) => a,
        (Value::Array(list), BoxView::Limited) => Value::Array(
            list.into_iter()
                .map(|a| match a {
                    Value::Object(a) => Value::Object(
                        a.into_iter()
                            .filter(|(k, _)| LIMITED_AGENT.contains(&k.as_str()))
                            .collect::<Map<String, Value>>(),
                    ),
                    _ => Value::Null,
                })
                .collect(),
        ),
        (_, BoxView::Limited) => json!([]),
    }
}

/// The hub's own box's agents: probed by the hub ([`AgentProbe`]), the last result kept.
pub(super) struct LocalAgents {
    probe: Option<AgentProbe>,
    /// The last probe's result (`None`: none ended yet).
    known: Mutex<Option<Vec<AgentInfo>>>,
    /// Held by the probe running (one at a time).
    probing: tokio::sync::Mutex<()>,
}

impl LocalAgents {
    pub(super) fn new(probe: Option<AgentProbe>) -> LocalAgents {
        LocalAgents {
            probe,
            known: Mutex::new(None),
            probing: tokio::sync::Mutex::new(()),
        }
    }

    /// Whether the hub probes its own agents at all.
    pub(super) fn probes(&self) -> bool {
        self.probe.is_some()
    }

    /// The agents as last probed (none before the first probe ended, or without a probe).
    pub(super) fn known(&self) -> Vec<AgentInfo> {
        self.known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// Probes the agents now (after a probe under way ends) and keeps the result; without a
    /// probe, what is known (nothing).
    pub(super) async fn probe(&self) -> Vec<AgentInfo> {
        let Some(probe) = &self.probe else {
            return self.known();
        };
        let _one = self.probing.lock().await;
        let agents = probe().await;
        tracing::debug!(
            found = agents.iter().filter(|a| a.found).count(),
            of = agents.len(),
            "probed the hub's own agent CLIs"
        );
        *self.known.lock().unwrap_or_else(|e| e.into_inner()) = Some(agents.clone());
        agents
    }
}

/// The hub's own probe in the background: at once, then every `every`, until the hub stops
/// running (a probe under way is dropped then, its commands killed).
pub(super) async fn local_ticker(
    agents: Arc<LocalAgents>,
    mut state: watch::Receiver<HubState>,
    every: Duration,
) {
    loop {
        tokio::select! {
            _ = agents.probe() => {}
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        }
    }
}

/// Asks box `rbox` for its agents on connection `epoch` (see the module docs): `timing.first`
/// after the welcome (again as long while the box is not ready yet), then every
/// `timing.every`, until that connection is no longer the box's current one.
pub(super) async fn poll(rbox: RemoteBox, epoch: u64, timing: AgentsPoll) {
    let mut wait = timing.first;
    loop {
        tokio::time::sleep(wait).await;
        if !rbox.is_current(epoch) {
            return;
        }
        if !rbox.is_ready() {
            wait = timing.first;
            continue;
        }
        match rbox
            .refresh_agents(false, AGENTS_TIMEOUT, Some(epoch))
            .await
        {
            Ok(a) => tracing::debug!(
                box_name = %rbox.name(),
                found = a.iter().filter(|a| a.found).count(),
                of = a.len(),
                "the box's agent CLIs"
            ),
            Err(e) => tracing::info!(
                box_name = %rbox.name(),
                error = %e,
                "the box did not say which agent CLIs it has"
            ),
        }
        wait = timing.every;
    }
}

/// Aborts its task when dropped (the agents poll of a connection that ended).
pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a thread on built-in agent runtime `runtime` on box `box_name` is warned of: the
/// box's last report says the runtime's CLI is not found there (see the module docs). `None`
/// for any other runtime, or when the box reported it found, or nothing about it.
pub(super) fn agent_warning(inner: &Inner, box_name: &str, runtime: &str) -> Option<String> {
    let (_, command, _) = twr_adapters::resolve::BUILTIN_AGENTS
        .iter()
        .find(|(r, ..)| *r == runtime)?;
    let agents = if box_name == LOCAL_BOX {
        inner.local_agents.known()
    } else {
        inner.remote(box_name)?.agents()
    };
    agents
        .iter()
        .find(|a| a.runtime == runtime)
        .filter(|a| !a.found)?;
    Some(if box_name == LOCAL_BOX {
        format!(
            "box {LOCAL_BOX} (the hub's machine) reports no `{command}` on its PATH; the thread \
             may fail to start"
        )
    } else {
        format!(
            "box {box_name} reports no `{command}` on its PATH; the thread may fail to start: \
             run `twr box doctor {box_name}`"
        )
    })
}

impl Hub {
    /// The hub's own box's agents as last probed (none before the first probe ended, or when
    /// the hub probes none).
    pub(super) fn local_agents_json(&self, view: BoxView) -> Value {
        project_agents(json!(self.inner.local_agents.known()), view)
    }

    /// `box.agents {box, refresh?}`: box `name`'s agent CLIs (`local`: the hub's own) as
    /// `view` sees them, `{box, online, agents}`; with `refresh` (users only: `unauthorized`
    /// otherwise) probed again first (a remote box's runner probes, waited for at most
    /// [`AGENTS_TIMEOUT`]; `box_offline` when it cannot be asked or does not answer).
    /// `not_found` for a box the hub does not have.
    pub async fn box_agents(
        &self,
        name: &str,
        refresh: bool,
        view: BoxView,
    ) -> Result<Value, RpcError> {
        if refresh && view != BoxView::Full {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user has a box probe its agents again",
            ));
        }
        let (online, agents) = if name == LOCAL_BOX {
            let agents = match refresh {
                true => self.inner.local_agents.probe().await,
                false => self.inner.local_agents.known(),
            };
            (true, agents)
        } else {
            let Some(rbox) = self.inner.remote(name) else {
                return Err(rpc(
                    codes::NOT_FOUND,
                    format!("the hub has no box {name:?}"),
                ));
            };
            let agents = match refresh {
                true => rbox
                    .refresh_agents(true, AGENTS_TIMEOUT, None)
                    .await
                    .map_err(|e| {
                        rpc(
                            codes::BOX_OFFLINE,
                            format!("box {name} could not probe its agents: {}", e.message),
                        )
                    })?,
                false => rbox.agents(),
            };
            let online = self.inner.core().online_boxes.contains(name);
            (online, agents)
        };
        Ok(json!({
            "box": name,
            "online": online,
            "agents": project_agents(json!(agents), view),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::runner::AgentLogin;

    fn agent() -> AgentInfo {
        AgentInfo {
            runtime: "claude-code".into(),
            found: true,
            path: Some("/home/me/.local/bin/claude".into()),
            version: Some("2.1.302 (Claude Code)".into()),
            login: AgentLogin::LoggedIn,
            method: Some("claude.ai".into()),
            note: Some("a note".into()),
        }
    }

    /// The orchestrator and agents see a box's state and which agents it has, signed in or
    /// not; a user sees all of it.
    #[test]
    fn the_limited_view_keeps_state_and_agents_only() {
        let b = json!({
            "name": "gpu", "transport": "ssh", "state": "online", "online": true,
            "version": "0.2.0", "refused": {"at": "x", "reason": "r"},
            "last_error": "/home/me/err", "notes": ["/home/me/.local/share/twr"],
            "os": "Linux", "arch": "x86_64", "build": "0123456789abcdef",
            "running_build": "0123456789abcdef", "switch_pending": false,
            "agents": [agent()],
            "something_new": 1,
        });
        let full = project_box(b.clone(), BoxView::Full);
        assert_eq!(full, b);
        let limited = project_box(b, BoxView::Limited);
        assert_eq!(
            limited,
            json!({
                "name": "gpu", "state": "online", "online": true, "version": "0.2.0",
                "agents": [{"runtime": "claude-code", "found": true, "login": "logged_in"}],
            })
        );
        assert_eq!(
            project_agents(json!(null), BoxView::Limited),
            json!([]),
            "nothing it does not know"
        );
    }
}

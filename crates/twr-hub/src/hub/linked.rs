//! Linked runner supervisor. Only the hub possesses K; the service relays sealed bytes.
use super::{Hub, Inner};
use crate::remote::RemoteBox;
use std::{sync::Arc, time::Duration};
use twr_core::paths::Paths;

pub(super) fn paths(inner: &Inner) -> Option<Paths> {
    Some(Paths {
        data_dir: inner.db_path.parent()?.to_path_buf(),
        socket: inner.socket.clone()?,
        config_file: inner.config_file.clone().unwrap_or_default(),
        workspace: inner.workspace.clone(),
    })
}
pub(super) fn start(inner: &Arc<Inner>, rbox: RemoteBox) {
    let weak = Arc::downgrade(inner);
    tokio::spawn(async move {
        let mut backoff = 1;
        let mut replace = false;
        loop {
            let Some(inner) = weak.upgrade() else { return };
            if *inner.state.borrow() != super::HubState::Running {
                return;
            }
            let Some(paths) = paths(&inner) else { return };
            let hub = Hub {
                inner: inner.clone(),
            };
            let name = rbox.name().to_string();
            let mut state = inner.state.subscribe();
            let connected = {
                let connection = linked_session(hub.clone(), paths.clone(), name.clone(), replace);
                tokio::pin!(connection);
                let mut capacity = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        result=&mut connection=>break result,
                        _=state.changed()=>return,
                        _=capacity.tick()=>{
                            if rbox.is_ready() && hub.retire_idle_linked_runtime(&name,false) {
                                if let Ok(pressure)=twr_link::bootstrap::request(&paths,&twr_link::service::Request::Pressure{name:name.clone(),workspace:paths.workspace_name().into(),release:None}).await {
                                    if pressure["waiting"]==true && !hub.retire_idle_linked_runtime(&name,true) {
                                        if let Some(claim)=pressure["claim"].as_str() {
                                            let _=twr_link::bootstrap::request(&paths,&twr_link::service::Request::Pressure{name:name.clone(),workspace:paths.workspace_name().into(),release:Some(claim.into())}).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };
            match connected {
                Ok(next) => {
                    replace = next;
                    backoff = 1;
                }
                Err(e) => {
                    tracing::warn!(box_name=%name,error=%format!("{e:#}"),"linked runner disconnected")
                }
            }
            drop(hub);
            drop(inner);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
}
async fn linked_session(
    hub: Hub,
    paths: Paths,
    name: String,
    replace: bool,
) -> anyhow::Result<bool> {
    let executable = match &hub.inner.link_program {
        Some(program) => program.clone(),
        None => std::env::current_exe()?,
    };
    let (io, _root, wanted) =
        twr_link::bootstrap::open(&paths, &name, &executable, replace).await?;
    let key = hub.box_key().await?;
    let timeout = if replace {
        crate::ssh_box::REPLACE_OPEN_TIMEOUT
    } else {
        crate::ssh_box::PLAIN_OPEN_TIMEOUT
    };
    let (sealed, hello) =
        tokio::time::timeout(timeout, twr_core::sealed::hub_side(io, &key, &name)).await??;
    let needs_switch = hello.build.as_deref() != Some(wanted.as_str());
    let ws = tokio_tungstenite::accept_async_with_config(
        sealed,
        Some(crate::listener::pre_auth_ws_config()),
    )
    .await?;
    let session = hub.serve_sealed_runner(ws, &name);
    tokio::pin!(session);
    let timing = hub.box_switch_timing();
    let mut check = tokio::time::interval(timing.check);
    let mut quiet: Option<(u64, tokio::time::Instant)> = None;
    let mut draining = None;
    loop {
        tokio::select! {
            end=&mut session=>{
                if end.protocol_refused.is_some(){let _=hub.begin_blind_box_switch(&name);return Ok(true)}
                return Ok(draining.is_some());
            },
            _=check.tick(),if needs_switch=>{
                if let Some(since)=draining {
                    if hub.box_switch_drained(&name)||tokio::time::Instant::now().duration_since(since)>=timing.drain {
                        hub.end_box_session(&name,"switching to the hub's exact worker build");return Ok(true)
                    }
                }else if let Ok(mark)=hub.box_switch_quiet(&name) {
                    let since=match quiet {Some((old,since)) if old==mark=>since,_=>{let now=tokio::time::Instant::now();quiet=Some((mark,now));now}};
                    if since.elapsed()>=timing.quiet&&hub.begin_box_switch(&name,mark){draining=Some(tokio::time::Instant::now());}
                }else{quiet=None;}
            }
        }
    }
}

impl Hub {
    pub async fn runner_control(
        &self,
        method: &str,
        params: serde_json::Value,
        caller: &super::Caller,
    ) -> Result<serde_json::Value, twr_core::proto::RpcError> {
        use twr_core::{
            config::{BoxConfig, BoxTransport, BoxesFile},
            proto::{codes, RpcError},
        };
        let error = |e: anyhow::Error| RpcError::new(codes::INTERNAL, format!("{e:#}"));
        if caller.is_orchestrator() {
            return Err(RpcError::new(
                codes::UNAUTHORIZED,
                "runner pairing and management are user-only",
            ));
        }
        let paths = paths(&self.inner).ok_or_else(|| {
            RpcError::new(codes::UNSUPPORTED, "hub has no local runner service paths")
        })?;
        let request = match method {
            "runner.manage" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Manage {
                    name: String,
                    admin: twr_link::service::Admin,
                }
                let p: Manage = serde_json::from_value(params).map_err(|_| {
                    RpcError::new(
                        codes::BAD_REQUEST,
                        "runner name and management action are required",
                    )
                })?;
                twr_link::service::Request::Manage {
                    name: p.name,
                    workspace: paths.workspace_name().into(),
                    admin: p.admin,
                }
            }
            "runner.status" => twr_link::service::Request::Status,
            "runner.connect" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Pair {
                    code: String,
                    #[serde(default)]
                    replace: bool,
                    name: String,
                }
                let p: Pair = serde_json::from_value(params).map_err(|_| {
                    RpcError::new(codes::BAD_REQUEST, "runner code and name are required")
                })?;
                twr_core::runner::validate_box_name(&p.name).map_err(error)?;
                let boxes = BoxesFile::load(&paths.boxes_file()).map_err(error)?;
                if boxes.boxes.contains_key(&p.name) {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        "a box already uses this name",
                    ));
                }
                twr_link::service::Request::Connect {
                    code: p.code,
                    replace: p.replace,
                    name: p.name,
                    workspace: paths.workspace_name().into(),
                }
            }
            "runner.recover" => twr_link::service::Request::Recover {
                name: params["name"].as_str().unwrap_or_default().into(),
                workspace: paths.workspace_name().into(),
            },
            "runner.revoke" => twr_link::service::Request::Revoke {
                machine: params["machine"].as_str().unwrap_or_default().into(),
            },
            "runner.forget" => twr_link::service::Request::Forget {
                name: params["name"].as_str().unwrap_or_default().into(),
            },
            _ => {
                return Err(RpcError::new(
                    codes::METHOD_NOT_FOUND,
                    "unknown runner action",
                ))
            }
        };
        if method != "runner.status" {
            if let Some(binary) = self.inner.link_program.as_deref() {
                twr_link::bootstrap::start(&paths, binary)
                    .await
                    .map_err(error)?;
            }
        }
        let result = twr_link::bootstrap::request(&paths, &request)
            .await
            .map_err(error)?;
        if method == "runner.connect" || method == "runner.recover" {
            let name = result["name"]
                .as_str()
                .ok_or_else(|| RpcError::new(codes::INTERNAL, "paired runner name is missing"))?
                .to_string();
            let file = paths.boxes_file();
            let n = name.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let _lock = BoxesFile::lock(&file)?;
                let mut boxes = BoxesFile::load(&file)?;
                if let Some(existing) = boxes.boxes.get(&n) {
                    anyhow::ensure!(
                        existing.transport() == BoxTransport::Link,
                        "a different box already uses this name"
                    );
                } else {
                    boxes.boxes.insert(
                        n,
                        BoxConfig {
                            transport: Some(BoxTransport::Link),
                            ..Default::default()
                        },
                    );
                    boxes.save(&file)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| error(e.into()))?
            .map_err(error)?;
            self.box_add(&name, caller).await?;
        }
        Ok(result)
    }
}

impl Hub {
    pub(super) async fn linked_prepare_project(
        &self,
        request: &mut super::NewThread,
    ) -> Result<(), twr_core::proto::RpcError> {
        use twr_core::{
            config::BoxTransport,
            proto::{codes, RpcError},
        };
        let name = super::threads::thread_box_name(request, &self.inner.config);
        if !self
            .inner
            .remote(&name)
            .is_some_and(|b| b.transport() == BoxTransport::Link)
        {
            return Ok(());
        }
        if request.no_worktree {
            return Err(RpcError::new(
                codes::BAD_REQUEST,
                "linked runners require a managed worktree",
            ));
        }
        let source = self
            .inner
            .config
            .projects
            .get(&request.project)
            .ok_or_else(|| {
                RpcError::new(
                    codes::NOT_FOUND,
                    "configure this project on the hub before using it on a linked runner",
                )
            })?
            .repo
            .clone();
        let paths = paths(&self.inner)
            .ok_or_else(|| RpcError::new(codes::UNSUPPORTED, "hub has no service paths"))?;
        let result = twr_link::bootstrap::request(
            &paths,
            &twr_link::service::Request::PushRepo {
                name: name.clone(),
                workspace: paths.workspace_name().into(),
                project: request.project.clone(),
                repository: source,
                base: request.base.clone(),
            },
        )
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("repository sync failed: {e:#}")))?;
        let repo = std::path::PathBuf::from(result["repo"].as_str().ok_or_else(|| {
            RpcError::new(codes::INTERNAL, "runner did not return repository paths")
        })?);
        let commit = result["commit"]
            .as_str()
            .filter(|id| twr_link::bundle::valid_oid(id))
            .ok_or_else(|| {
                RpcError::new(codes::INTERNAL, "runner did not confirm an exact commit")
            })?;
        request.base = Some(commit.into());
        request.worktree = true;
        self.inner.set_link_project(&name, &request.project, repo)?;
        Ok(())
    }
    pub async fn thread_pull(
        &self,
        query: &str,
        caller: &super::Caller,
    ) -> Result<serde_json::Value, twr_core::proto::RpcError> {
        use twr_core::{
            config::BoxTransport,
            proto::{codes, RpcError},
        };
        if caller.is_orchestrator() {
            return Err(RpcError::new(
                codes::UNAUTHORIZED,
                "pulling committed results is user-only",
            ));
        }
        let thread = self.get_thread(query)?;
        let name = thread["box_name"].as_str().unwrap_or("local");
        if !self
            .inner
            .remote(name)
            .is_some_and(|b| b.transport() == BoxTransport::Link)
        {
            return Err(RpcError::new(
                codes::UNSUPPORTED,
                "thread pull is for linked runners",
            ));
        }
        let project = thread["project"]
            .as_str()
            .ok_or_else(|| RpcError::new(codes::NOT_FOUND, "thread has no project"))?;
        let repository = self
            .inner
            .config
            .projects
            .get(project)
            .ok_or_else(|| {
                RpcError::new(
                    codes::NOT_FOUND,
                    "configure this project on the hub before pulling results",
                )
            })?
            .repo
            .clone();
        let branch = thread["branch"]
            .as_str()
            .ok_or_else(|| RpcError::new(codes::NOT_FOUND, "thread has no committed branch"))?;
        let id = thread["id"]
            .as_str()
            .ok_or_else(|| RpcError::new(codes::NOT_FOUND, "thread identity is missing"))?;
        let paths = paths(&self.inner)
            .ok_or_else(|| RpcError::new(codes::UNSUPPORTED, "hub has no service paths"))?;
        twr_link::bootstrap::request(
            &paths,
            &twr_link::service::Request::PullRepo {
                name: name.into(),
                workspace: paths.workspace_name().into(),
                project: project.into(),
                repository,
                branch: branch.into(),
                result: id.into(),
            },
        )
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL, format!("{e:#}")))
    }
}

impl Hub {
    /// Reclaim one known resumable runtime, with the same durable stop machinery as a
    /// worker switch. The selection and stop request happen under one core lock.
    fn retire_idle_linked_runtime(&self, name: &str, retire: bool) -> bool {
        use twr_core::event::BOX_CAPACITY_REASON;
        let mut core = self.inner.core();
        if core.shutting_down
            || !core.online_boxes.contains(name)
            || core.switching_boxes.contains(name)
        {
            return false;
        }
        if self
            .inner
            .remote(name)
            .is_some_and(|r| r.repository_changes() > 0 || r.ops_in_progress() > 0)
        {
            return false;
        }
        let candidate = core
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == name)
            .find_map(|v| {
                if !matches!(v.runtime.as_str(), "claude-code" | "codex" | "grok-build")
                    || v.session_id.is_none()
                    || v.current_turn.is_some()
                    || v.runtime_tasks()
                    || v.attached.is_some()
                {
                    return None;
                }
                let rt = core.rt.get(&v.id)?;
                if !rt.live.as_ref().is_some_and(|live| live.run_confirmed)
                    || rt.stopping()
                    || rt.attach.is_some()
                    || rt.park.is_some()
                    || rt.resuming
                    || rt.deleting
                    || rt.trimming
                {
                    return None;
                }
                if !core
                    .open_adapter_attention(&v.id, super::AdapterScope::All, false)
                    .is_empty()
                    || core.store.pending_count(&v.id).ok() != Some(0)
                    || !matches!(core.due_retry(&v.id), Ok(None))
                {
                    return None;
                }
                Some(v.id.clone())
            });
        if !retire {
            return candidate.is_some();
        }
        if let Some(thread) = candidate {
            if let Ok(stopped) = core.stop_box_runtimes(&self.inner, name, vec![thread]) {
                let retired = !stopped.is_empty();
                for thread in stopped {
                    core.rt(&thread).restart_reason = Some(BOX_CAPACITY_REASON);
                }
                return retired;
            }
        }
        false
    }
}

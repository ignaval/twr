//! Private stdio bridge. Uses the same peer-credential check, protocol handshake, fold hash,
//! and reducers as twr's CLI. Losing this process never stops the independent hub.
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};
use twr_core::{
    client::Client,
    client_state::{ClientFold, FOLD},
    config::Config,
    paths::Paths,
};

#[derive(Deserialize)]
struct Call {
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}
// Each bridge serves one workspace. Reuse authenticated RPC connections; keep
// streams on their own sockets and permit concurrent long operations.
static RPC_CLIENTS: std::sync::OnceLock<tokio::sync::Mutex<Vec<Client>>> =
    std::sync::OnceLock::new();
fn rpc_clients() -> &'static tokio::sync::Mutex<Vec<Client>> {
    RPC_CLIENTS.get_or_init(|| tokio::sync::Mutex::new(Vec::new()))
}
static DEVICE_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

async fn client(socket: &std::path::Path) -> Result<Client> {
    let mut c = Client::connect(socket).await?;
    let name = std::env::var("TWR_CLIENT_NAME")
        .ok()
        .filter(|s| s.starts_with("mobile-") && s.len() <= 64)
        .unwrap_or_else(|| "desktop".to_owned());
    let mut params = json!({"client":name});
    if let Some(token) = DEVICE_TOKEN.get() {
        params["device_token"] = json!(token);
    }
    let hello = c.hello(params).await?;
    // Never disguise an agent-launched process as a user. The hub remains authoritative.
    anyhow::ensure!(hello["identity"].as_str().is_some_and(|s| s.starts_with("user:")),
        "The hub did not recognize this desktop process as your user. Open twr from your desktop launcher.");
    Ok(c)
}

async fn control(
    socket: PathBuf,
    tx: mpsc::Sender<Value>,
    state: watch::Sender<Value>,
    presentation: Arc<std::sync::Mutex<twr_app::Presentation>>,
) {
    loop {
        let result: Result<()> = async {
            let mut c = client(&socket).await?;
            let mut sub = c
                .subscribe(
                    "control.subscribe",
                    json!({"since":0,"compact":true,"snapshot":true}),
                )
                .await?;
            let mut fold = ClientFold::default();
            if let Some(snapshot) = sub.ack().get("snapshot") {
                if sub.ack()["snapshot_fold"] == FOLD {
                    fold =
                        serde_json::from_value(snapshot.clone()).context("Reading hub snapshot")?;
                } else {
                    c = client(&socket).await?;
                    sub = c
                        .subscribe("control.subscribe", json!({"since":0,"compact":true}))
                        .await?;
                }
            }
            tx.send(json!({"type":"connection","connected":true}))
                .await?;
            presentation.lock().unwrap().retry_requests.extend(fold.capped.keys().cloned());
            // Which remote boxes are online now; their runners' events keep it after.
            if let Ok(Ok(info)) = tokio::time::timeout(Duration::from_secs(5), async {
                client(&socket).await?.call("hub.info", json!({})).await
            })
            .await
            {
                presentation.lock().unwrap().take_boxes(&info);
            }
            let mut displayed = presentation.lock().unwrap().project(&fold);
            state.send_replace(displayed.clone());
            let mut hydrate = client(&socket).await?;
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                let item = tokio::select! { item = sub.next() => item?, _ = tick.tick() => {
                    let retry = {let mut p=presentation.lock().unwrap();let id=p.retry_requests.iter().next().cloned();if let Some(id)=&id{p.retry_requests.remove(id);}id};
                    if let Some(id)=retry {
                        if let Some((revision,event))=fold.capped.get(&id).cloned() {
                            let item=twr_core::proto::StreamItem{stream:twr_core::proto::StreamKind::Control,gseq:Some(revision),event};
                            match tokio::time::timeout(Duration::from_secs(5),twr_core::blob::hydrate_item(&item,&mut hydrate)).await {
                                Ok(Ok(Some(full))) if fold.attention.get(&id).is_some_and(|a|a.revision==revision) => {fold.attention.apply_at(&full.event,Some(revision));fold.capped.remove(&id);},
                                _ => {let _=tx.send(json!({"type":"error","error":"The full request is still unavailable. It has not been answered."})).await;if let Ok(c)=client(&socket).await{hydrate=c;}},
                            }
                        }
                    }
                    let next = presentation.lock().unwrap().project(&fold);
                    if next != displayed { displayed = next; state.send_replace(displayed.clone()); }
                    continue;
                }};
                let Some(mut item) = item else { break; };
                // Blobs include full request summaries; never approve only a truncated preview.
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    twr_core::blob::hydrate_item(&item, &mut hydrate),
                )
                .await
                {
                    Ok(Ok(Some(full))) => item = full,
                    Ok(Ok(None)) => {}
                    _ => {
                        if let Ok(c) = client(&socket).await {
                            hydrate = c;
                        }
                    }
                }
                fold.apply(&item.event, item.gseq);
                presentation.lock().unwrap().runner_event(&item.event);
                displayed = presentation.lock().unwrap().project(&fold);
                state.send_replace(displayed.clone());
                tx.send(json!({"type":"control","event":item.event,"gseq":item.gseq}))
                    .await?;
            }
            anyhow::bail!("Hub connection closed; reconnecting")
        }
        .await;
        if let Err(e) = result {
            rpc_clients().lock().await.clear();
            if tx
                .send(json!({"type":"connection","connected":false,"error":e.to_string()}))
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn looking_link(socket: PathBuf, mut wanted: watch::Receiver<bool>) {
    loop {
        let result: Result<()> = async {
            let mut link = client(&socket).await?;
            loop {
                let looking = *wanted.borrow_and_update();
                tokio::time::timeout(
                    Duration::from_secs(5),
                    link.call("notify.looking", json!({"looking":looking})),
                )
                .await??;
                if wanted.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
        .await;
        if result.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_call(
    call: Call,
    socket: PathBuf,
    paths: Arc<Paths>,
    tx: mpsc::Sender<Value>,
    streams: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
    >,
    presentation: Arc<std::sync::Mutex<twr_app::Presentation>>,
    looking: watch::Sender<bool>,
) {
    let result: Result<Value> = async {
        if std::env::var_os("TWR_BRIDGE_READ_ONLY").is_some() {
            anyhow::ensure!(matches!(call.method.as_str(), "hub.info" | "attention.list" | "thread.list"), "Inbox connections only read workspace summaries");
        }
        if call.method == "desktop.cli_plan" {
            let args: Vec<String> = serde_json::from_value(call.params["args"].clone())?;
            let workspace = call.params["workspace"].as_str().context("Workspace required")?;
            return Ok(json!(twr_core::app_commands::plan(&args, workspace)?));
        }
        if call.method == "desktop.parse_slash" {
            let line = call.params["line"].as_str().context("Command line required")?;
            return serde_json::to_value(twr_core::slash::parse(line).map_err(anyhow::Error::msg)?).map_err(Into::into);
        }
        if call.method == "desktop.slash_commands" {
            return Ok(json!(twr_core::slash::COMMANDS.iter().map(|(name,_,description,_)|json!({"id":name.trim_start_matches('/'),"title":description})).collect::<Vec<_>>()));
        }
        if call.method == "desktop.retry_attention" {
            let id=call.params["attn_id"].as_str().context("Request ID required")?;
            anyhow::ensure!(id.len()<=512,"Invalid request ID");
            presentation.lock().unwrap().retry_requests.insert(id.to_string());
            return Ok(json!({"ok":true}));
        }
        if call.method == "notify.looking" {
            looking.send_replace(call.params["looking"].as_bool().unwrap_or(false));
            return Ok(json!({"ok":true}));
        }
        if call.method == "desktop.preferences" {
            let mut p = presentation.lock().unwrap();
            if let Some(pins) = call.params["pins"].as_array() { p.pins = pins.iter().filter_map(|s| s.as_str().map(String::from)).take(1000).collect(); }
            p.held = call.params["held"].as_bool().unwrap_or(false);
            return Ok(json!({"ok":true}));
        }
        if call.method == "desktop.paths" {
            return Ok(json!({"config":paths.config_file,"data":paths.data_dir,"socket":socket,"workspace":paths.workspace_name(),"version":twr_core::version(),"fold":FOLD}));
        }
        if call.method == "desktop.runner.use" || call.method == "desktop.runner.cancel" || call.method == "desktop.runner.status" {
            anyhow::ensure!(tokio::task::spawn_blocking(||twr_hub::local_agent(std::process::id()).is_none()).await?,"Runner controls are user-only");
            if call.method=="desktop.runner.use" {
                let binary=PathBuf::from(call.params["binary"].as_str().context("Bundled executable required")?);
                twr_link::bootstrap::start(&paths,&binary).await?;
            }
            let request=match call.method.as_str(){"desktop.runner.use"=>twr_link::service::Request::UseAsRunner,"desktop.runner.cancel"=>twr_link::service::Request::CancelPairing{session:call.params["session"].as_str().map(String::from)},_=>twr_link::service::Request::Status};
            // These paths identify the desktop device, including while viewing a remote hub.
            return twr_link::bootstrap::request(&paths,&request).await;
        }
        if call.method == "desktop.ensure_hub" {
            let config = Config::load_from(&paths)?;
            anyhow::ensure!(config.socket_path(&paths) == socket, "Configuration changed the socket. Reconnect to the workspace to use it.");
            let binary = PathBuf::from(call.params["binary"].as_str().context("Bundled executable required")?);
            return twr_app::runtime::ensure_hub(&paths, &socket, &binary).await;
        }
        if call.method == "desktop.config.set" {
            use twr_core::config_edit::Scalar;
            let table: Vec<String> = serde_json::from_value(call.params["table"].clone())?;
            let key = call.params["key"].as_str().context("Setting key required")?;
            let value = match &call.params["value"] {
                Value::Bool(v) => Scalar::Bool(*v),
                Value::String(v) => Scalar::Str(v.clone()),
                Value::Number(v) => Scalar::Int(v.as_i64().context("Setting must be an integer")?),
                _ => anyhow::bail!("Setting must be a scalar"),
            };
            let table: Vec<&str> = table.iter().map(String::as_str).collect();
            twr_core::config_edit::write_key(&paths.config_file, &table, key, &value).map_err(anyhow::Error::msg)?;
            return Ok(json!({"ok":true}));
        }
        if call.method == "desktop.config.read" || call.method == "desktop.config.save" {
            if call.method == "desktop.config.read" {
                let content = match twr_core::config::read_locked(&paths.config_file, twr_core::config::CONFIG_LOCK_WAIT) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                let settings = Config::from_toml_str(&content).ok().map(|c| {
                    presentation.lock().unwrap().card_order = c.tui.card_order;
                    json!({
                    "default_runtime":c.default_runtime,
                    "threads":{"default_mode":c.threads.default_mode,"worktree":c.threads.worktree,"model":c.threads.model,"thinking":c.threads.thinking,"idle_compaction":c.threads.idle_compaction},
                    "trash":{"keep_days":c.trash.keep_days},"log":{"retention_days":c.log.retention_days,"archive":c.log.archive},
                    "stall_after_secs":c.stall_after_secs,"background_timeout_secs":c.background_timeout_secs,"tui":{"card_order":c.tui.card_order,"suggest":c.tui.suggest,"suggest_model":c.tui.suggest_model,"summaries":c.tui.summaries,"summary_every_secs":c.tui.summary_every().as_secs(),"summary_every":c.tui.summary_every_span}
                })});
                return Ok(json!({"content":content,"hash":hex::encode(Sha256::digest(content.as_bytes())),"path":paths.config_file,"settings":settings}));
            }
            let content = call.params["content"].as_str().context("Configuration text required")?;
            let hash = call.params["hash"].as_str().context("Configuration revision required")?;
            twr_core::config_edit::write_text(&paths.config_file, content, hash).map_err(anyhow::Error::msg)?;
            return Ok(json!({"ok":true}));
        }
        if call.method == "desktop.subscribe" {
            let thread = call.params["thread"].as_str().context("Thread required")?.to_string();
            let since = call.params["since"].as_u64().unwrap_or(0);
            let subscription = call.params["subscription"].as_str().context("Subscription ID required")?.to_string();
            let mut map = streams.lock().await;
            // One visible transcript at a time. Replacing a subscription closes its socket.
            let mut sub = client(&socket).await?.subscribe("thread.subscribe", json!({"thread":thread,"since_seq":since})).await?;
            let stream_tx = tx.clone();
            let mut hydrate = client(&socket).await?;
            let stream_thread = thread.clone();
            let stream_socket = socket.clone();
            let handle = tokio::spawn(async move {
                let result: Result<()> = async {
                    while let Some(mut item) = sub.next().await? {
                        match tokio::time::timeout(Duration::from_secs(5), twr_core::blob::hydrate_item(&item, &mut hydrate)).await {
                            Ok(Ok(Some(full))) => item = full,
                            Ok(Ok(None)) => {},
                            _ => { if let Ok(c) = client(&stream_socket).await { hydrate = c; } }
                        }
                        let mut event=serde_json::to_value(&item.event)?;
                        twr_app::present_event(&mut event);
                        stream_tx.send(json!({"type":"thread","thread":stream_thread,"subscription":subscription,"event":event})).await?;
                    }
                    anyhow::bail!("Transcript connection closed")
                }.await;
                if let Err(e) = result { let _ = stream_tx.send(json!({"type":"transcript-error","thread":stream_thread,"subscription":subscription,"error":e.to_string()})).await; }
            });
            for (_, handle) in map.drain() { handle.abort(); }
            map.insert(thread, handle);
            return Ok(json!({"ok":true}));
        }
        twr_app::validate_call(&call.method, &call.params)?;
        if matches!(call.method.as_str(), "attention.respond" | "attention.dismiss") {
            let id = call.params["attn_id"].as_str().context("Request ID required")?;
            anyhow::ensure!(!presentation.lock().unwrap().incomplete.contains(id), "Load and review the full request before answering it.");
        }
        let cached = rpc_clients().lock().await.pop();
        let mut c = if let Some(c) = cached { c } else { client(&socket).await? };
        let mut params = call.params;
        if call.method == "memory.save" && params["rev"].as_str().is_none() {
            match c.call("memory.read", json!({"name":params["name"]})).await {
                Ok(_) => anyhow::bail!("This note already exists. Open it with Read / edit so its current revision is preserved."),
                Err(error) if error.downcast_ref::<twr_core::proto::RpcError>().is_some_and(|e| e.code == twr_core::proto::codes::NOT_FOUND) => {},
                Err(error) => return Err(error),
            }
        }
        if call.method == "thread.new" {
            let cwd = params.as_object_mut().and_then(|params| params.remove("desktop_cwd")).and_then(|value| value.as_str().map(String::from));
            if let Some(project) = params["project"].as_str() {
                params["project"] = json!(if let Some(cwd) = &cwd {
                    let base = std::path::Path::new(cwd);
                    anyhow::ensure!(base.is_absolute() && base.is_dir(), "Choose an existing absolute working directory.");
                    twr_core::paths::project_param_at(project, base)
                } else { twr_core::paths::project_param(project) });
            }
        }
        if call.method == "usage.threads" {
            if let Some(since) = params["since"].as_str() {
                params["since"] = json!(twr_core::usage::parse_since(since, std::time::SystemTime::now().into())?.to_rfc3339());
            }
        }
        let result = c.call(&call.method, params.clone()).await;
        let mut result = match result {
            Ok(value) => value,
            Err(error) if error.downcast_ref::<twr_core::client::HubClosed>().is_some_and(twr_core::client::HubClosed::resendable)
                || (error.downcast_ref::<twr_core::proto::RpcError>().is_none()
                && twr_core::devices::method_scope(&call.method) == Some("read")) => {
                // Retry reads, or a mutation only when the protocol proves it
                // was never executed. Uncertain mutations are never retried.
                c = client(&socket).await?;
                c.call(&call.method, params).await?
            }
            Err(error) => return Err(error),
        };
        // Hydrate historical transcript/event payloads as well as live subscriptions.
        if matches!(call.method.as_str(), "thread.events" | "thread.transcript") {
            if let Some(events) = result["events"].as_array_mut() {
                let original=std::mem::take(events);
                let total=original.len();
                let mut bytes=0;
                for mut ev in original {
                    if twr_core::blob::has_capped(&ev["data"]) {
                        let _ = tokio::time::timeout(Duration::from_secs(5), twr_core::blob::hydrate_deep(&mut ev["data"], &mut c)).await;
                    }
                    twr_app::present_event(&mut ev);
                    let size=serde_json::to_vec(&ev)?.len();
                    if !events.is_empty() && bytes+size>1024*1024 {break;}
                    bytes+=size;
                    events.push(ev);
                }
                if events.len()<total {
                    let next=events.last().and_then(|e|e["seq"].as_u64()).context("Transcript event sequence missing")?;
                    result["more"]=json!(true);
                    result["next"]=json!(next);
                    result["last_seq"]=json!(next);
                }
            }
        }
        let mut idle = rpc_clients().lock().await;
        if idle.len() < 8 { idle.push(c); }
        Ok(result)
    }.await;
    let response = match result {
        Ok(v) => json!({"id":call.id,"result":v}),
        Err(e) => {
            json!({"id":call.id,"error":format!("{e:#}"),"rpc_error":e.downcast_ref::<twr_core::proto::RpcError>()})
        }
    };
    let _ = tx.send(response).await;
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything starts a hub: it must not inherit the app's crash handler.
    twr_app::runtime::forget_inherited_crash_handler();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    if std::env::var_os("TWR_MOBILE_TRANSPORT").is_some() {
        let line = lines
            .next_line()
            .await?
            .context("Mobile transport initialization required")?;
        anyhow::ensure!(
            line.len() <= 1024,
            "Invalid mobile transport initialization"
        );
        let init: Value = serde_json::from_str(&line)?;
        let token = init["device_token"]
            .as_str()
            .context("Device credential required")?;
        anyhow::ensure!(token.len() == 64, "Invalid device credential");
        let _ = DEVICE_TOKEN.set(token.to_owned());
    }
    let workspace = std::env::args().nth(1).unwrap_or_else(|| "default".into());
    let paths = Arc::new(Paths::from_env_in(Some(&workspace))?);
    let config = Config::load_from(&paths);
    let socket = config
        .as_ref()
        .map(|c| c.socket_path(&paths))
        .unwrap_or_else(|_| paths.socket.clone());
    let (tx, mut rx) = mpsc::channel::<Value>(256);
    let (state_tx, mut state_rx) = watch::channel(Value::Null);
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(value) = rx.recv().await {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            out.write_all(&line).await?;
            out.flush().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    if let Err(error) = config {
        let _ = tx
            .send(json!({"type":"config-error","error":format!("{error:#}")}))
            .await;
    }
    let read_only = std::env::var_os("TWR_BRIDGE_READ_ONLY").is_some();
    let state_out = tx.clone();
    let state_pump = (!read_only).then(|| {
        tokio::spawn(async move {
            while state_rx.changed().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(40)).await;
                let view = state_rx.borrow_and_update().clone();
                if state_out
                    .send(json!({"type":"state","state":view}))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    });
    let presentation = Arc::new(std::sync::Mutex::new(twr_app::Presentation::default()));
    let control = (!read_only).then(|| {
        tokio::spawn(control(
            socket.clone(),
            tx.clone(),
            state_tx,
            presentation.clone(),
        ))
    });
    let streams = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let (looking_tx, looking_rx) = watch::channel(false);
    let looking = (!read_only).then(|| tokio::spawn(looking_link(socket.clone(), looking_rx)));
    let mut calls = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        if line.len() > 16 * 1024 * 1024 {
            let _ = tx
                .send(json!({"type":"error","error":"Desktop request too large"}))
                .await;
            continue;
        }
        match serde_json::from_str::<Call>(&line) {
            Ok(call) => {
                // Bound in-flight work from the host. Never retry a mutation on a lost reply.
                if calls.len() >= 64 {
                    let _ = tx
                        .send(json!({"id":call.id,"error":"Too many pending operations"}))
                        .await;
                    continue;
                }
                calls.spawn(run_call(
                    call,
                    socket.clone(),
                    paths.clone(),
                    tx.clone(),
                    streams.clone(),
                    presentation.clone(),
                    looking_tx.clone(),
                ));
            }
            Err(e) => {
                let _ = tx
                    .send(json!({"type":"error","error":format!("Invalid desktop request: {e}")}))
                    .await;
            }
        }
        while calls.try_join_next().is_some() {}
    }
    if let Some(task) = control {
        task.abort();
    }
    if let Some(task) = looking {
        task.abort();
    }
    if let Some(task) = state_pump {
        task.abort();
    }
    calls.abort_all();
    for (_, task) in streams.lock().await.drain() {
        task.abort();
    }
    drop(tx);
    writer.abort();
    Ok(())
}

//! Authenticated remote user transport. Runner tokens are never accepted here.
mod remote;
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use remote::{
    auth::{Auth, Device},
    companion::Companion,
    terminal::Terminal,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock, Semaphore};

#[derive(Parser)]
#[command(
    about = "Remote mobile clients for twr. Start from your own terminal or desktop launcher."
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:47321")]
    listen: SocketAddr,
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    #[arg(long)]
    tls_key: Option<PathBuf>,
    /// Debug builds only: plaintext loopback for adb reverse and iOS Simulator.
    #[arg(long)]
    development: bool,
    /// Print a new one-use pairing code, valid for five minutes.
    #[arg(long)]
    pair: bool,
    /// Create a code for an already running gateway, then exit.
    #[arg(long, requires = "pair")]
    pair_only: bool,
    #[arg(long)]
    allow_terminal: bool,
    #[arg(long)]
    allow_admin: bool,
    #[arg(long)]
    revoke: Option<String>,
    #[arg(long)]
    list_devices: bool,
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    #[arg(long)]
    push_config: Option<PathBuf>,
    #[arg(long)]
    push_test_log: Option<PathBuf>,
}
struct Server {
    pairing_window: std::sync::Mutex<(std::time::Instant, u32)>,
    terminal_slots: Arc<Semaphore>,
    binary: PathBuf,
    companion: PathBuf,
    owner: Mutex<Option<twr_core::client::Client>>,
    auth: Auth,
    slots: Arc<Semaphore>,
    development: bool,
    sessions: Mutex<HashMap<String, Arc<DeviceSession>>>,
    push: Arc<remote::push::Push>,
    observers: Mutex<std::collections::HashSet<String>>,
    receipts: remote::receipts::Receipts,
    receipt_waiters: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<Value>>>>,
}
type OrderedInput = (Call, mpsc::Sender<Value>, u64);
fn stamp(mut event: Value, generation: u64) -> Value {
    event["_bridge_generation"] = json!(generation);
    event
}
fn event_generation(event: &Value) -> Option<u64> {
    event["_bridge_generation"].as_u64()
}
fn outbound(mut event: Value, fragmented: bool) -> remote::wire::Outbound {
    let generation = event
        .as_object_mut()
        .and_then(|o| o.remove("_bridge_generation"))
        .and_then(|v| v.as_u64());
    remote::wire::Outbound::new(socket_message(&event), generation, fragmented)
}

fn authorize_mutation(device: &Device, call: &Call) -> Result<()> {
    let allowed = match call.method.as_str() {
        "rpc" => device.allows_params(
            call.params["method"].as_str().unwrap_or(""),
            &call.params["params"],
        ),
        "config-save" | "config-set" => device.scopes.iter().any(|s| s == "admin"),
        "terminal-open" => device.scopes.iter().any(|s| s == "terminal"),
        "cli" => device.allows("thread.new"),
        _ => false,
    };
    anyhow::ensure!(
        allowed,
        "This device does not have permission for this operation"
    );
    Ok(())
}

const MAX_SOCKET_REPLY: usize = 16 * 1024 * 1024;

/// Keep a large transcript or state event from tearing down a healthy native socket.
fn send_budget(message: &Message) -> Duration {
    let bytes = match message {
        Message::Text(text) => text.len(),
        Message::Binary(data) => data.len(),
        _ => 0,
    };
    Duration::from_secs(20 + (bytes as u64).div_ceil(32 * 1024))
}
fn socket_message(event: &Value) -> Message {
    let text = event.to_string();
    let text = if text.len() > MAX_SOCKET_REPLY {
        let error =
            "Hub response exceeds 16 MiB. Load a smaller transcript page or open it on desktop.";
        match event.get("id") {
            Some(id) => json!({"id":id,"error":error}).to_string(),
            None => json!({"type":"error","error":error}).to_string(),
        }
    } else {
        text
    };
    Message::Text(text.into())
}
#[cfg(test)]
mod socket_tests {
    use super::*;
    #[test]
    fn denied_and_unknown_rpc_calls_cannot_claim_receipts() {
        let mut device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: "read only".into(),
            created: 0,
            token_hash: String::new(),
            scopes: vec!["read".into()],
        };
        let call = Call {
            id: uuid::Uuid::new_v4().to_string(),
            method: "rpc".into(),
            params: json!({"method":"thread.send","params":{}}),
            workspace: None,
        };
        assert!(authorize_mutation(&device, &call).is_err());
        device.scopes.push("operate".into());
        assert!(authorize_mutation(&device, &call).is_ok());
        let unknown = Call {
            params: json!({"method":"unknown"}),
            ..call
        };
        assert!(authorize_mutation(&device, &unknown).is_err());
    }
    #[test]
    fn large_frames_get_a_slow_link_transfer_budget() {
        let frame = socket_message(&json!({"id":"large","result":"x".repeat(4*1024*1024)}));
        assert!(send_budget(&frame) >= Duration::from_secs(148));
        assert!(
            send_budget(&socket_message(&json!({"id":"small","result":true})))
                <= Duration::from_secs(21)
        );
    }
    #[test]
    fn oversized_replies_and_events_become_bounded_errors() {
        for value in [
            json!({"id":"request-id","result":"界".repeat(MAX_SOCKET_REPLY / 3 + 1)}),
            json!({"type":"state","state":"x".repeat(MAX_SOCKET_REPLY)}),
        ] {
            let Message::Text(text) = socket_message(&value) else {
                panic!("text frame required")
            };
            assert!(text.len() < MAX_SOCKET_REPLY);
            let result: Value = serde_json::from_str(&text).unwrap();
            assert!(result["error"].as_str().unwrap().contains("16 MiB"));
            if value.get("id").is_some() {
                assert_eq!(result["id"], value["id"]);
            }
        }
        let small = json!({"id":"next","result":{"ok":true}});
        let Message::Text(text) = socket_message(&small) else {
            panic!("text frame required")
        };
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), small);
    }
}
async fn owner_client() -> Result<twr_core::client::Client> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
        let config = twr_core::config::Config::load_from(&paths)?;
        let mut client = twr_core::client::Client::connect(&config.socket_path(&paths)).await?;
        let hello = client.hello(json!({"client":"mobile-gateway"})).await?;
        anyhow::ensure!(
            hello["identity"]
                .as_str()
                .is_some_and(|id| id.starts_with("user:")),
            "Launch the gateway from your terminal or desktop launcher"
        );
        Ok(client)
    })
    .await
    .context("The hub did not authenticate the gateway within 30 seconds")?
}
impl Server {
    async fn owner_call(&self, method: &str, params: Value) -> Result<Value> {
        let mut owner = self
            .owner
            .try_lock()
            .context("The hub is busy with another device operation; try again")?;
        for attempt in 0..2 {
            if owner.is_none() {
                *owner = Some(owner_client().await?);
            }
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                owner.as_mut().unwrap().call(method, params.clone()),
            )
            .await;
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    *owner = None;
                    anyhow::bail!("The hub did not answer within 30 seconds; the operation may have completed")
                }
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retry = error
                        .downcast_ref::<twr_core::client::HubClosed>()
                        .is_some_and(twr_core::client::HubClosed::resendable);
                    if error.downcast_ref::<twr_core::proto::RpcError>().is_none() {
                        *owner = None;
                    }
                    if attempt == 0 && retry {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        unreachable!()
    }
}
struct DeviceSession {
    input: mpsc::Sender<OrderedInput>,
    session: Session,
    events: broadcast::Sender<Value>,
    terminal_events: broadcast::Sender<Value>,
    latest_state: Arc<std::sync::Mutex<Option<Value>>>,
    generation: AtomicU64,
    last_seen: AtomicU64,
    foreground: AtomicBool,
}
type Shared = Arc<Server>;
#[derive(Deserialize)]
struct Pair {
    code: String,
    name: String,
}
async fn pair(State(server): State<Shared>, Json(p): Json<Pair>) -> impl IntoResponse {
    {
        let mut window = server.pairing_window.lock().unwrap();
        if window.0.elapsed() > Duration::from_secs(60) {
            *window = (std::time::Instant::now(), 0);
        }
        window.1 += 1;
        if window.1 > 20 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":"Too many pairing attempts; wait a minute"})),
            );
        }
    }
    if ![10, 64].contains(&p.code.len()) || p.name.is_empty() || p.name.len() > 80 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Invalid pairing request"})),
        );
    }
    let result = server
        .owner_call("device.pair_complete", json!({"code":p.code,"name":p.name}))
        .await;
    match result {
        Ok(mut value) => {
            value["gateway_id"] = json!(server.push.gateway_id);
            (StatusCode::OK, Json(value))
        }
        Err(error)
            if error
                .downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|e| e.code == twr_core::proto::codes::UNAUTHORIZED) =>
        {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Pairing code invalid or expired"})),
            )
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"The hub is unavailable or busy. Try again when it reconnects."})),
        ),
    }
}
async fn upgrade(
    State(server): State<Shared>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().unwrap_or("");
        if ![
            "capacitor://localhost",
            "http://localhost",
            "https://localhost",
        ]
        .contains(&origin)
            && !(server.development
                && (origin.starts_with("http://127.0.0.1:")
                    || origin.starts_with("http://localhost:")))
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let Ok(device) = server.auth.authenticate(token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let token = token.to_owned();
    let fragmented = headers.get("x-twr-framing").and_then(|v| v.to_str().ok()) == Some("1");
    let Ok(slot) = server.slots.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.max_message_size(2 * 1024 * 1024)
        .max_frame_size(2 * 1024 * 1024)
        .on_upgrade(move |ws| async move {
            let _slot = slot;
            let _ = serve_socket(server, ws, device, token, fragmented).await;
        })
}
#[derive(Deserialize)]
struct Call {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
    workspace: Option<String>,
}
struct WorkspaceConnection {
    workspace: String,
    bridge: Option<Arc<Companion>>,
}
struct Session {
    terminal_slots: Arc<Semaphore>,
    device: Device,
    token: String,
    connection: RwLock<WorkspaceConnection>,
    connecting: Mutex<()>,
    bridge_generation: Arc<AtomicU64>,
    terminals: Mutex<HashMap<String, Terminal>>,
    events: mpsc::Sender<Value>,
    terminal_events: mpsc::Sender<Value>,
    uploads: Mutex<HashMap<String, remote::upload::Upload>>,
    upload_quota: std::sync::Mutex<remote::upload::Quota>,
    latest_state: Arc<std::sync::Mutex<Option<Value>>>,
}
impl Session {
    async fn call(&self, server: &Server, call: &Call) -> Result<Value> {
        let p = &call.params;
        let connection = self.connection.read().await;
        let workspace = connection.workspace.clone();
        let selected_bridge = connection.bridge.clone();
        drop(connection);
        let bridge = || {
            selected_bridge
                .clone()
                .context("Connect to a workspace first")
        };
        anyhow::ensure!(
            matches!(call.method.as_str(), "connect" | "bootstrap")
                || call
                    .workspace
                    .as_ref()
                    .is_none_or(|requested| requested == &workspace),
            "Workspace changed; review the destination before retrying"
        );
        match call.method.as_str() {
            "bootstrap" => Ok(
                json!({"workspace":workspace,"platform":"remote","home":std::env::var("HOME").unwrap_or_default(),"version":twr_core::version(),"theme":null,"terminals":self.terminals.lock().await.values().map(Terminal::summary).collect::<Vec<_>>(),"scopes":self.device.scopes,"allowed_methods":twr_core::devices::METHOD_SCOPES.iter().flat_map(|(_,m)|m.iter()).filter(|m|self.device.allows(m) && !m.starts_with("device.pair_")).collect::<Vec<_>>(),"read_methods":twr_core::devices::METHOD_SCOPES.iter().find(|(s,_)|*s=="read").map(|(_,m)|*m).unwrap_or_default()}),
            ),
            "connect" => {
                let _connecting = self.connecting.lock().await;
                let workspace = p["workspace"].as_str().context("Workspace required")?;
                if let Some(error) = twr_core::paths::workspace_name_error(workspace) {
                    anyhow::bail!(error);
                }
                let existing = {
                    let connection = self.connection.read().await;
                    (connection.workspace == workspace)
                        .then(|| connection.bridge.clone())
                        .flatten()
                };
                if let Some(bridge) = existing.filter(|bridge| bridge.is_alive()) {
                    if bridge.call("hub.info", json!({})).await.is_ok() {
                        let paths = bridge.call("desktop.paths", json!({})).await?;
                        let cached = self.latest_state.lock().unwrap().clone();
                        if let Some(state) = cached {
                            if event_generation(&state)
                                == Some(self.bridge_generation.load(Ordering::SeqCst))
                            {
                                let _ = self.events.send(state).await;
                            }
                        }
                        let _ = self
                            .events
                            .send(json!({"type":"connection","connected":true}))
                            .await;
                        return Ok(paths);
                    }
                }
                let generation = self.bridge_generation.load(Ordering::SeqCst) + 1;
                if self.device.scopes.iter().any(|s| s == "operate") {
                    let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
                    let config = twr_core::config::Config::load_from(&paths)?;
                    twr_app::runtime::ensure_hub(
                        &paths,
                        &config.socket_path(&paths),
                        &server.binary,
                    )
                    .await?;
                }
                let (events, mut received) = mpsc::channel::<Value>(256);
                let output = self.events.clone();
                let current = self.bridge_generation.clone();
                let (ready, mut committed) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let mut initial_state = None;
                    let mut initial_connection = None;
                    let mut initial_other = std::collections::VecDeque::new();
                    // Drain before commit so events cannot block the stdout reader
                    // that also delivers the authentication RPC replies.
                    loop {
                        tokio::select! {biased;
                            result=&mut committed=>{if result.is_err(){return;}break;},
                            event=received.recv()=>{
                                let Some(event)=event else{return;};
                                match event["type"].as_str() {
                                    Some("state")=>initial_state=Some(event),
                                    Some("connection")=>initial_connection=Some(event),
                                    Some("control")=>{},
                                    _=>{if initial_other.len()==32{initial_other.pop_front();}initial_other.push_back(event);}
                                }
                            }
                        }
                    }
                    for event in initial_connection
                        .into_iter()
                        .chain(initial_state)
                        .chain(initial_other)
                    {
                        if current.load(Ordering::SeqCst) == generation
                            && output.send(stamp(event, generation)).await.is_err()
                        {
                            return;
                        }
                    }
                    while let Some(event) = received.recv().await {
                        if current.load(Ordering::SeqCst) == generation
                            && output.send(stamp(event, generation)).await.is_err()
                        {
                            break;
                        }
                    }
                });
                let bridge = Companion::start(
                    &server.companion,
                    workspace,
                    &self.device.id,
                    &self.token,
                    events,
                )
                .await?;
                let paths = bridge.call("desktop.paths", json!({})).await?;
                // Prove device identity before presenting the connection as usable.
                bridge.call("hub.info", json!({})).await?;
                self.bridge_generation.store(generation, Ordering::SeqCst);
                self.latest_state.lock().unwrap().take();
                *self.connection.write().await = WorkspaceConnection {
                    workspace: workspace.into(),
                    bridge: Some(Arc::new(bridge)),
                };
                let _ = ready.send(());
                Ok(paths)
            }
            "rpc" => {
                let method = p["method"].as_str().context("Method required")?;
                let private = [
                    "desktop.subscribe",
                    "desktop.preferences",
                    "desktop.retry_attention",
                    "desktop.parse_slash",
                    "desktop.slash_commands",
                ];
                anyhow::ensure!(
                    !method.starts_with("desktop.") || private.contains(&method),
                    "Private host operation"
                );
                anyhow::ensure!(
                    self.device.allows_params(method, &p["params"]),
                    "This device does not have permission for this operation"
                );
                bridge()?.call(method, p["params"].clone()).await
            }
            "catalog" => Ok(serde_json::from_str(include_str!(
                "../../../desktop/mobile/catalog.json"
            ))?),
            "directories" => {
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "operate"),
                    "Browsing directories requires operate permission"
                );
                let path = PathBuf::from(p["path"].as_str().context("Directory required")?);
                anyhow::ensure!(
                    path.is_absolute() && path.as_os_str().len() <= 4096,
                    "An absolute directory is required"
                );
                tokio::task::spawn_blocking(move||->Result<Value>{let path=std::fs::canonicalize(path)?;let mut entries=Vec::new();let mut truncated=false;for entry in std::fs::read_dir(&path)?{let entry=entry?;if !entry.file_type()?.is_dir(){continue}
if entries.len()>=200{truncated=true;break}entries.push(json!({"name":entry.file_name().to_string_lossy(),"path":entry.path()}));}entries.sort_by(|a,b|a["name"].as_str().cmp(&b["name"].as_str()));Ok(json!({"path":path,"parent":path.parent(),"entries":entries,"truncated":truncated}))}).await?
            }
            "unpair" => {
                server
                    .owner_call("device.revoke", json!({"id":self.device.id}))
                    .await
            }
            "push-status" => Ok(server.push.status()),
            "push-unregister" => {
                server.push.unregister(&self.device.id).await?;
                Ok(json!({"ok":true}))
            }
            "push-register" => {
                server
                    .push
                    .register(remote::push::Recipient {
                        device: self.device.id.clone(),
                        workspace: workspace.clone(),
                        platform: p["platform"]
                            .as_str()
                            .context("Push platform required")?
                            .to_owned(),
                        token: p["token"]
                            .as_str()
                            .context("Push token required")?
                            .to_owned(),
                    })
                    .await?;
                Ok(json!({"ok":true}))
            }
            "upload-begin" => {
                anyhow::ensure!(
                    self.device.allows("thread.send"),
                    "Uploading requires operate permission"
                );
                anyhow::ensure!(
                    self.uploads.lock().await.len() < 4,
                    "Finish an existing upload first"
                );
                let size = usize::try_from(p["size"].as_u64().context("File size required")?)?;
                anyhow::ensure!(
                    size <= remote::upload::MAX_FILE,
                    "Files can be at most 25 MiB"
                );
                let thread = bridge()?
                    .call("thread.get", json!({"thread":p["thread"]}))
                    .await?;
                anyhow::ensure!(thread["box_name"].as_str()==Some("local"),"File upload to a worker on another box is not available yet. Choose a thread running on this hub.");
                let root = thread["worktree"]
                    .as_str()
                    .or(thread["repo"].as_str())
                    .context("Thread has no working directory")?;
                let name = p["name"].as_str().context("Filename required")?;
                let upload = remote::upload::Upload::begin(
                    Path::new(root),
                    name,
                    size,
                    p["sha256"].as_str().context("Checksum required")?,
                )?;
                let out = tokio::process::Command::new("git")
                    .args([
                        "-c",
                        "core.fsmonitor=false",
                        "-C",
                        root,
                        "rev-parse",
                        "--is-inside-work-tree",
                        "--git-path",
                        "info/exclude",
                    ])
                    .env("LC_ALL", "C")
                    .output()
                    .await?;
                let not_git = !out.status.success()
                    && String::from_utf8_lossy(&out.stderr).contains("not a git repository");
                anyhow::ensure!(
                    out.status.success() || not_git,
                    "Could not check the upload directory's git status"
                );
                let output = String::from_utf8(out.stdout)?;
                if out.status.success() && output.lines().next() == Some("true") {
                    let exclude =
                        PathBuf::from(output.lines().nth(1).context("Git exclude path missing")?);
                    let exclude = if exclude.is_absolute() {
                        exclude
                    } else {
                        Path::new(root).join(exclude)
                    };
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    if let Some(parent) = exclude.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .append(true)
                        .create(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(exclude)?;
                    use std::io::Read;
                    let mut contents = String::new();
                    (&mut file).take(65536).read_to_string(&mut contents)?;
                    if !contents.lines().any(|line| line == ".twr-uploads/") {
                        file.write_all(b"\n.twr-uploads/\n")?;
                    }
                }
                let id = uuid::Uuid::new_v4().to_string();
                let mut uploads = self.uploads.lock().await;
                anyhow::ensure!(uploads.len() < 4, "Finish an existing upload first");
                self.upload_quota
                    .lock()
                    .unwrap()
                    .reserve(&id, size, std::time::Instant::now())?;
                uploads.insert(id.clone(), upload);
                Ok(json!({"id":id,"chunk_size":262144}))
            }
            "upload-chunk" => {
                let id = p["id"].as_str().context("Upload ID required")?;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(p["data"].as_str().context("File data required")?)?;
                let count = self
                    .uploads
                    .lock()
                    .await
                    .get_mut(id)
                    .context("Upload not found")?
                    .chunk(&data)?;
                Ok(json!({"received":count}))
            }
            "upload-commit" => {
                let id = p["id"].as_str().unwrap_or("");
                let upload = self
                    .uploads
                    .lock()
                    .await
                    .remove(id)
                    .context("Upload not found")?;
                match upload.commit() {
                    Ok(path) => Ok(json!({"path":path})),
                    Err(error) => {
                        self.upload_quota.lock().unwrap().release(id);
                        Err(error)
                    }
                }
            }
            "upload-cancel" => {
                let id = p["id"].as_str().unwrap_or("");
                remote::upload::cancel(
                    &mut *self.uploads.lock().await,
                    &mut self.upload_quota.lock().unwrap(),
                    id,
                );
                Ok(Value::Null)
            }
            "cli" => {
                let args: Vec<String> = serde_json::from_value(p["args"].clone())?;
                anyhow::ensure!(
                    args.first().is_some_and(|s| s == "ws")
                        && args.get(1).is_some_and(|s| s == "list" || s == "new"),
                    "Use an embedded terminal for this command"
                );
                if args[1] == "new" {
                    anyhow::ensure!(self.device.allows("thread.new"), "Device is read-only");
                }
                validate_args(&args)?;
                let out = tokio::process::Command::new(&server.binary)
                    .args(["-w", &workspace])
                    .args(args)
                    .env_remove("TWR_DEVICE_TOKEN")
                    .output()
                    .await?;
                Ok(
                    json!({"stdout":String::from_utf8_lossy(&out.stdout),"stderr":String::from_utf8_lossy(&out.stderr),"code":out.status.code().unwrap_or(1)}),
                )
            }
            "config-read" | "config-save" | "config-set" => {
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "admin"),
                    "Hub settings require admin permission for this device"
                );
                let method = match call.method.as_str() {
                    "config-read" => "desktop.config.read",
                    "config-save" => "desktop.config.save",
                    _ => "desktop.config.set",
                };
                bridge()?.call(method, p.clone()).await
            }
            "terminal-open" => {
                let device_permit = self
                    .terminal_slots
                    .clone()
                    .try_acquire_owned()
                    .context("Close a terminal first (maximum four)")?;
                let server_permit = server
                    .terminal_slots
                    .clone()
                    .try_acquire_owned()
                    .context("The hub has eight active mobile terminals. Close one first.")?;
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "terminal"),
                    "Remote terminals require terminal permission for this device"
                );
                anyhow::ensure!(
                    self.terminals
                        .lock()
                        .await
                        .values()
                        .filter(|t| !t.exited.load(Ordering::SeqCst))
                        .count()
                        < 4,
                    "Close a terminal first (maximum four)"
                );
                let shell = p["shell"].as_bool().unwrap_or(false);
                let args: Vec<String> = p
                    .get("args")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let args = if shell {
                    vec!["-l".to_owned()]
                } else {
                    twr_core::app_commands::plan(&args, &workspace)?
                };
                let binary = if shell {
                    PathBuf::from(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()))
                } else {
                    server.binary.clone()
                };
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let cwd = Path::new(p["cwd"].as_str().unwrap_or(&home));
                anyhow::ensure!(
                    cwd.is_absolute() && cwd.is_dir(),
                    "Choose an existing absolute directory on the hub"
                );
                if !shell {
                    let out = tokio::process::Command::new(&binary)
                        .arg("--desktop-validate")
                        .args(&args)
                        .output()
                        .await?;
                    anyhow::ensure!(
                        out.status.success(),
                        "{}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                let terminal = Terminal::open(
                    &binary,
                    &args,
                    cwd,
                    p["title"].as_str().unwrap_or("Remote terminal").to_owned(),
                    workspace.clone(),
                    self.terminal_events.clone(),
                    vec![device_permit, server_permit],
                )?;
                let summary = terminal.summary();
                let mut terminals = self.terminals.lock().await;
                if terminals.len() >= 16 {
                    let oldest = terminals
                        .iter()
                        .filter(|(_, t)| t.exited.load(Ordering::SeqCst))
                        .min_by_key(|(_, t)| t.started)
                        .map(|(id, _)| id.clone());
                    if let Some(id) = oldest {
                        terminals.remove(&id);
                        let _ = self
                            .events
                            .try_send(json!({"type":"terminal-removed","id":id}));
                    }
                }
                terminals.insert(terminal.id.clone(), terminal);
                Ok(summary)
            }
            "terminal-read" => Ok(self
                .terminals
                .lock()
                .await
                .get(p["id"].as_str().unwrap_or(""))
                .context("Terminal not found")?
                .read()),
            "terminal-write" => {
                self.terminals
                    .lock()
                    .await
                    .get_mut(p["id"].as_str().unwrap_or(""))
                    .context("Terminal not found")?
                    .write(p["data"].as_str().context("Input required")?)
                    .await?;
                Ok(Value::Null)
            }
            "terminal-resize" => {
                self.terminals
                    .lock()
                    .await
                    .get(p["id"].as_str().unwrap_or(""))
                    .context("Terminal not found")?
                    .resize(
                        u16::try_from(p["cols"].as_u64().unwrap_or(0))?,
                        u16::try_from(p["rows"].as_u64().unwrap_or(0))?,
                    )?;
                Ok(Value::Null)
            }
            "terminal-close" => {
                self.terminals
                    .lock()
                    .await
                    .remove(p["id"].as_str().unwrap_or(""));
                Ok(Value::Null)
            }
            "terminal-ack" | "terminal-focus" => Ok(Value::Null),
            _ => anyhow::bail!("Unsupported mobile operation"),
        }
    }
}
fn validate_args(args: &[String]) -> Result<()> {
    twr_core::app_commands::validate(args)
}
async fn serve_socket(
    server: Shared,
    mut socket: WebSocket,
    device: Device,
    token: String,
    fragmented: bool,
) -> Result<()> {
    socket
        .send(Message::Text(
            json!({"type":"authenticated","device":device.id,"scopes":device.scopes,"framing":fragmented})
                .to_string()
                .into(),
        ))
        .await?;
    let slot = {
        let mut sessions = server.sessions.lock().await;
        if let Some(slot) = sessions.get(&device.id) {
            slot.clone()
        } else {
            let (tx, mut rx) = mpsc::channel::<Value>(128);
            let (events, _) = broadcast::channel(256);
            let (terminal_tx, mut terminal_rx) = mpsc::channel::<Value>(128);
            let (terminal_events, _) = broadcast::channel(256);
            let terminal_forward = terminal_events.clone();
            tokio::spawn(async move {
                while let Some(event) = terminal_rx.recv().await {
                    let _ = terminal_forward.send(event);
                }
            });
            let latest_state = Arc::new(std::sync::Mutex::new(None));
            let state_cache = latest_state.clone();
            let bridge_generation = Arc::new(AtomicU64::new(0));
            let current_generation = bridge_generation.clone();
            let forward = events.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if event_generation(&event)
                        .is_some_and(|g| g != current_generation.load(Ordering::SeqCst))
                    {
                        continue;
                    }
                    if event["type"] == "state" {
                        let mut cache = state_cache.lock().unwrap();
                        if event_generation(&event)
                            == Some(current_generation.load(Ordering::SeqCst))
                        {
                            *cache = Some(event.clone());
                        }
                    }
                    let _ = forward.send(event);
                }
            });
            let (input, mut input_rx) = mpsc::channel::<OrderedInput>(64);
            let slot = Arc::new(DeviceSession {
                input,
                session: Session {
                    terminal_slots: Arc::new(Semaphore::new(4)),
                    device: device.clone(),
                    token: token.clone(),
                    connection: RwLock::new(WorkspaceConnection {
                        workspace: "default".into(),
                        bridge: None,
                    }),
                    connecting: Mutex::new(()),
                    bridge_generation,
                    terminals: Mutex::new(HashMap::new()),
                    events: tx,
                    terminal_events: terminal_tx,
                    uploads: Mutex::new(HashMap::new()),
                    upload_quota: std::sync::Mutex::new(remote::upload::Quota::default()),
                    latest_state: latest_state.clone(),
                },
                events,
                terminal_events,
                latest_state,
                generation: AtomicU64::new(0),
                last_seen: AtomicU64::new(remote::auth::now()),
                foreground: AtomicBool::new(false),
            });
            let weak_slot = Arc::downgrade(&slot);
            let weak_server = Arc::downgrade(&server);
            tokio::spawn(async move {
                while let Some((call, replies, generation)) = input_rx.recv().await {
                    let (Some(slot), Some(server)) = (weak_slot.upgrade(), weak_server.upgrade())
                    else {
                        break;
                    };
                    if slot.generation.load(Ordering::SeqCst) != generation
                        || !server.auth.current(&slot.session.device)
                    {
                        continue;
                    }
                    let result = slot.session.call(&server, &call).await;
                    let reply = match result {
                        Ok(value) => json!({"id":call.id,"result":value}),
                        Err(error) => json!({"id":call.id,"error":error.to_string()}),
                    };
                    let _ = replies.send(reply).await;
                }
            });
            sessions.insert(device.id.clone(), slot.clone());
            slot
        }
    };
    let generation = slot.generation.fetch_add(1, Ordering::SeqCst) + 1;
    slot.last_seen.store(remote::auth::now(), Ordering::SeqCst);
    let (mut sink, mut incoming) = socket.split();
    let mut rx = slot.events.subscribe();
    let mut terminal_rx = slot.terminal_events.subscribe();
    let state_cache = slot.latest_state.clone();
    let (replies, mut response_rx) = mpsc::channel::<Value>(32);
    let alive = Arc::new(AtomicBool::new(true));
    let connected = alive.clone();
    let (heartbeat_tx, mut heartbeat_rx) = mpsc::channel::<Value>(16);
    let bridge_epoch = slot.session.bridge_generation.clone();
    let writer = tokio::spawn(async move {
        use remote::wire::Outbound;
        // Two independent RPC replies, one ordered control stream and one
        // ordered PTY stream. Keepalives always pass between bounded frames.
        let mut lanes: [Option<Outbound>; 4] = [None, None, None, None];
        let mut cursor = 0;
        enum Ready {
            Heartbeat(Option<Value>),
            Reply(Option<Value>),
            Control(Result<Value, broadcast::error::RecvError>),
            Terminal(Result<Value, broadcast::error::RecvError>),
            Send,
        }
        loop {
            let ready = tokio::select! {biased;
                beat=heartbeat_rx.recv()=>Ready::Heartbeat(beat),
                reply=response_rx.recv(),if lanes[0].is_none()||lanes[1].is_none()=>Ready::Reply(reply),
                event=rx.recv(),if lanes[2].is_none()=>Ready::Control(event),
                event=terminal_rx.recv(),if lanes[3].is_none()=>Ready::Terminal(event),
                _=std::future::ready(()),if lanes.iter().any(Option::is_some)=>Ready::Send,
            };
            let immediate = match ready {
                Ready::Heartbeat(Some(event)) => Some(socket_message(&event)),
                Ready::Heartbeat(None) | Ready::Reply(None) => break,
                Ready::Reply(Some(event)) => {
                    let index = if lanes[0].is_none() { 0 } else { 1 };
                    lanes[index] = Some(outbound(event, fragmented));
                    None
                }
                Ready::Control(Ok(event)) => {
                    lanes[2] = Some(outbound(event, fragmented));
                    None
                }
                Ready::Control(Err(broadcast::error::RecvError::Lagged(_))) => {
                    let snapshot = state_cache.lock().unwrap().clone().filter(|e| {
                        event_generation(e) == Some(bridge_epoch.load(Ordering::SeqCst))
                    });
                    let mut message = outbound(
                        snapshot.unwrap_or_else(|| json!({"type":"resync"})),
                        fragmented,
                    );
                    message
                        .frames
                        .push_back(socket_message(&json!({"type":"resync"})));
                    lanes[2] = Some(message);
                    None
                }
                Ready::Terminal(Ok(event)) => {
                    lanes[3] = Some(outbound(event, fragmented));
                    None
                }
                Ready::Terminal(Err(broadcast::error::RecvError::Lagged(_))) => {
                    lanes[3] = Some(outbound(json!({"type":"terminal-resync"}), fragmented));
                    None
                }
                Ready::Control(Err(_)) | Ready::Terminal(Err(_)) => break,
                Ready::Send => {
                    let index = (0..4)
                        .map(|offset| (cursor + offset) % 4)
                        .find(|i| lanes[*i].is_some())
                        .unwrap();
                    cursor = (index + 1) % 4;
                    let current = lanes[index].as_mut().unwrap();
                    if current
                        .generation
                        .is_some_and(|g| g != bridge_epoch.load(Ordering::SeqCst))
                    {
                        let cancellation = current.cancel();
                        lanes[index] = None;
                        cancellation
                    } else {
                        let frame = current.frames.pop_front();
                        if current.frames.is_empty() {
                            lanes[index] = None;
                        }
                        frame
                    }
                }
            };
            if let Some(frame) = immediate {
                if !matches!(
                    tokio::time::timeout(send_budget(&frame), sink.send(frame)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        }
        connected.store(false, Ordering::SeqCst);
    });
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut count = 0;
    let mut window = std::time::Instant::now();
    let calls = Arc::new(Semaphore::new(64));
    let mut heartbeat = tokio::time::Instant::now();
    let mut last_activity = tokio::time::Instant::now();
    loop {
        tokio::select! {
                  _=tick.tick()=>{
                    if !alive.load(Ordering::SeqCst)||!server.auth.current(&device)||slot.generation.load(Ordering::SeqCst)!=generation||last_activity.elapsed()>Duration::from_secs(45){break}
                    slot.last_seen.store(remote::auth::now(),Ordering::SeqCst);
                    if heartbeat.elapsed()>Duration::from_secs(15){if heartbeat_tx.is_closed(){break}let _=heartbeat_tx.try_send(json!({"type":"heartbeat"}));heartbeat=tokio::time::Instant::now();}
                  }
                  message=incoming.next()=>{
                    let Some(Ok(message))=message else{break};
                    last_activity=tokio::time::Instant::now();
                    if !server.auth.current(&device)||slot.generation.load(Ordering::SeqCst)!=generation{break}
                    let Message::Text(text)=message else{continue};
                    let Ok(call)=serde_json::from_str::<Call>(&text)else{let _=replies.try_send(json!({"type":"error","error":"Invalid request"}));continue};
                    if uuid::Uuid::parse_str(&call.id).is_err()||call.method.len()>80 {let _=replies.try_send(json!({"type":"error","error":"Invalid request ID or method"}));continue}
                    if window.elapsed()>Duration::from_secs(60){window=std::time::Instant::now();count=0}
                    count+=1;
                    if count>1200{let _=replies.try_send(json!({"id":call.id,"error":"Too many requests. Wait a moment before trying again."}));continue}
                    if call.method=="heartbeat"{let _=heartbeat_tx.try_send(json!({"id":call.id,"result":null}));continue}

                    // One bounded ordered input lane. A blocked PTY cannot stall RPCs,
            // heartbeats or revocation on the WebSocket read loop.
            if matches!(call.method.as_str(),"terminal-write"|"terminal-resize") {
                let id=call.id.clone();
                if slot.input.try_send((call,replies.clone(),generation)).is_err() {let _=replies.try_send(json!({"id":id,"error":"Terminal input queue is full"}));}
                continue;
            }
            let Ok(permit)=calls.clone().try_acquire_owned()else{let _=replies.try_send(json!({"id":call.id,"error":"Too many pending operations"}));continue};
                    let task_server=server.clone();let task_slot=slot.clone();let task_device=device.clone();let task_replies=replies.clone();
                    tokio::spawn(async move{
                      let _permit=permit;
                      if !task_server.auth.current(&task_device)||task_slot.generation.load(Ordering::SeqCst)!=generation{return}
                      let mutation=remote::receipts::mutation(&call.method,&call.params);
                      let workspace=call.workspace.clone().unwrap_or(task_slot.session.connection.read().await.workspace.clone());
                      let receipt_key=format!("{}:{}",task_device.id,call.id);
                      let mut completion=None;
                      if mutation {
                        if let Err(error)=authorize_mutation(&task_device,&call) {let _=task_replies.send(json!({"id":call.id,"error":error.to_string()})).await;return;}
                        let mut waiters=task_server.receipt_waiters.lock().await;
                        match task_server.receipts.begin(&task_device.id,&call.id,&workspace,&call.method,&call.params){Ok(Some(reply))=>{
                          if let Some(mut receiver)=waiters.get(&receipt_key).cloned() {
                            drop(waiters);
                            let ready=tokio::time::timeout(Duration::from_secs(660),async {loop {if let Some(value)=receiver.borrow().clone(){return Some(value)}
        if receiver.changed().await.is_err(){return None}}}).await.ok().flatten();
                            let _=task_replies.send(ready.unwrap_or(reply)).await;
                          } else {drop(waiters);let _=task_replies.send(reply).await;}
                          return
                        },Ok(None)=>{let (sender,receiver)=tokio::sync::watch::channel(None);waiters.insert(receipt_key.clone(),receiver);completion=Some(sender);},Err(error)=>{drop(waiters);let _=task_replies.send(json!({"id":call.id,"error":error.to_string()})).await;return}}
                      }
                      let result=task_slot.session.call(&task_server,&call).await;
                      if result.is_ok()&&call.method=="connect"{let workspace=task_slot.session.connection.read().await.workspace.clone();start_observer(task_server.clone(),workspace).await;}
                      if result.is_ok()&&call.method=="rpc"&&call.params["method"]=="notify.looking"{task_slot.foreground.store(call.params["params"]["looking"].as_bool().unwrap_or(false),Ordering::SeqCst);}
                      let reply=match result{Ok(result)=>json!({"id":call.id,"result":result}),Err(error)=>json!({"id":call.id,"error":error.to_string()})};
                      if mutation{if let Err(error)=task_server.receipts.finish(&task_device.id,&call.id,&reply){eprintln!("Could not record request outcome: {error}");}}
                      if let Some(completion)=completion {completion.send_replace(Some(reply.clone()));task_server.receipt_waiters.lock().await.remove(&receipt_key);}
                      let _=task_replies.send(reply).await;
                    });
                  }
                }
    }
    if slot.generation.load(Ordering::SeqCst) == generation {
        slot.foreground.store(false, Ordering::SeqCst);
        if let Some(bridge) = slot.session.connection.read().await.bridge.as_ref() {
            let _ = bridge
                .call("notify.looking", json!({"looking":false}))
                .await;
        }
    }
    slot.last_seen.store(remote::auth::now(), Ordering::SeqCst);
    drop(replies);
    drop(heartbeat_tx);
    writer.abort();
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let administration = args.pair_only || args.list_devices || args.revoke.is_some();
    anyhow::ensure!(
        !args.development || (cfg!(debug_assertions) && args.listen.ip().is_loopback()),
        "Cleartext development transport requires a debug build on loopback"
    );
    anyhow::ensure!(
        administration || args.development || (args.tls_cert.is_some() && args.tls_key.is_some()),
        "TLS certificate and key are required. Debug loopback tests may use --development"
    );
    let dir = args.binary_dir.unwrap_or(
        std::env::current_exe()?
            .parent()
            .context("Binary directory")?
            .to_owned(),
    );
    let binary = dir.join("twr");
    let companion = dir.join("twr-desktop-bridge");
    let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
    let auth = Auth::open(&paths.config_file.with_file_name("mobile"))?;
    let config = twr_core::config::Config::load_from(&paths)?;
    twr_app::runtime::ensure_hub(&paths, &config.socket_path(&paths), &binary).await?;
    let mut local = owner_client().await?;
    if let Some(id) = args.revoke {
        local.call("device.revoke", json!({"id":id})).await?;
        return Ok(());
    }
    if args.list_devices {
        println!("{}", local.call("device.list", json!({})).await?);
        return Ok(());
    }
    if args.pair {
        let mut scopes = vec!["read", "operate"];
        if args.allow_terminal {
            scopes.push("terminal")
        }
        if args.allow_admin {
            scopes.push("admin")
        }
        let code = local
            .call("device.pair_begin", json!({"scopes":scopes}))
            .await?;
        println!(
            "Pairing code (one use, five minutes): {}",
            code["code"].as_str().unwrap_or("")
        );
        if args.pair_only {
            return Ok(());
        }
    }
    let push = remote::push::Push::new(
        &paths.config_file.with_file_name("mobile"),
        args.push_config.as_deref(),
        args.push_test_log,
    )?;
    let receipts = remote::receipts::Receipts::new(&paths.root().data_dir.join("mobile"))?;
    let server = Arc::new(Server {
        pairing_window: std::sync::Mutex::new((std::time::Instant::now(), 0)),
        terminal_slots: Arc::new(Semaphore::new(8)),
        binary,
        companion,
        owner: Mutex::new(Some(local)),
        auth,
        slots: Arc::new(Semaphore::new(32)),
        development: args.development,
        sessions: Mutex::new(HashMap::new()),
        push,
        observers: Mutex::new(std::collections::HashSet::new()),
        receipts,
        receipt_waiters: Mutex::new(HashMap::new()),
    });
    start_observer(server.clone(), "default".into()).await;
    for workspace in server.push.workspaces().await {
        start_observer(server.clone(), workspace).await;
    }
    let cleanup = server.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            let _ = cleanup.receipts.prune();
            let active: Vec<_> = cleanup.sessions.lock().await.values().cloned().collect();
            for slot in active {
                slot.session.uploads.lock().await.retain(|id, upload| {
                    let keep = upload.created.elapsed() < Duration::from_secs(900);
                    if !keep {
                        slot.session.upload_quota.lock().unwrap().release(id);
                    }
                    keep
                });
            }
            let mut sessions = cleanup.sessions.lock().await;
            sessions.retain(|id, slot| {
                let valid = cleanup
                    .auth
                    .devices()
                    .ok()
                    .and_then(|v| {
                        v.as_array()
                            .map(|a| a.iter().any(|d| d["id"].as_str() == Some(id)))
                    })
                    .unwrap_or(false);
                valid
                    && remote::auth::now().saturating_sub(slot.last_seen.load(Ordering::SeqCst))
                        < 1800
            });
        }
    });
    let router = Router::new()
        .route("/v1/pair", post(pair))
        .route("/v1/socket", get(upgrade))
        .route(
            "/health",
            get(|State(server):State<Shared>| async move { Json(json!({"service":"twr-mobile","protocol":1,"gateway_id":server.push.gateway_id})) }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .with_state(server);
    eprintln!("twr mobile gateway listening on {}", args.listen);
    if args.development {
        axum::serve(tokio::net::TcpListener::bind(args.listen).await?, router).await?;
    } else {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            args.tls_cert.unwrap(),
            args.tls_key.unwrap(),
        )
        .await?;
        axum_server::bind_rustls(args.listen, config)
            .serve(router.into_make_service())
            .await?;
    }
    Ok(())
}

async fn start_observer(server: Shared, workspace: String) {
    if !server.observers.lock().await.insert(workspace.clone()) {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = observe_push(&server, &workspace).await {
                eprintln!("Push observer for {workspace}: {error}");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}
async fn observe_push(server: &Server, workspace: &str) -> Result<()> {
    use twr_core::{
        client::Client,
        client_state::{ClientFold, FOLD},
        config::Config,
        notify::{still_due, Held, Policy},
    };
    let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
    let config = Config::load_from(&paths)?;
    let mut client = Client::connect(&config.socket_path(&paths)).await?;
    let hello = client.hello(json!({"client":"mobile-notify"})).await?;
    anyhow::ensure!(
        hello["identity"]
            .as_str()
            .is_some_and(|s| s.starts_with("user:")),
        "Push observer is not a user client"
    );
    let mut sub = client
        .subscribe(
            "control.subscribe",
            json!({"since":0,"compact":true,"snapshot":true}),
        )
        .await?;
    if sub.ack().get("snapshot").is_some() && sub.ack()["snapshot_fold"] != FOLD {
        let mut client = Client::connect(&config.socket_path(&paths)).await?;
        client.hello(json!({"client":"mobile-notify"})).await?;
        sub = client
            .subscribe("control.subscribe", json!({"since":0,"compact":true}))
            .await?;
    }
    let head = sub.ack()["head"].as_u64().unwrap_or(0);
    let mut fold: ClientFold = if let Some(snapshot) = sub.ack().get("snapshot") {
        serde_json::from_value(snapshot.clone())?
    } else {
        ClientFold::default()
    };
    let mut policy = Policy::adopt(&config.notify.events, &fold.policy);
    let mut held = Held::default();
    let mut last_sent = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
         item=sub.next()=>{let item=item?.context("Hub subscription closed")?;fold.apply(&item.event,item.gseq);if item.gseq.is_some_and(|g|g<=head){policy.recall(&item.event,item.gseq,&fold.registry,&fold.attention);}else if let Some(candidate)=policy.observe(&item.event,item.gseq,&fold.registry,&fold.attention){held.add(candidate,std::time::Instant::now());}}
         _=tick.tick()=>{
          let now = std::time::Instant::now();
          if held.next_due(last_sent).is_none_or(|due| due > now) {continue;}
          let mut due:Vec<_>=held.take_due(now).into_iter().filter(|c|still_due(c,&fold.registry,&fold.attention)).collect();
          due.sort_by_key(|c|c.event);
          if due.is_empty(){continue;}
          let slots:Vec<_>=server.sessions.lock().await.iter().map(|(id,slot)|(id.clone(),slot.clone())).collect();
          let mut exclude=std::collections::HashSet::new();
          for(id,slot)in slots {if slot.foreground.load(Ordering::SeqCst)&&slot.session.connection.read().await.workspace==workspace{exclude.insert(id);}}
          last_sent=Some(now);
          if let Err(error)=server.push.deliver(&server.auth,workspace,&exclude,&due).await{eprintln!("Push delivery failed: {error}");}

         }
        }
    }
}

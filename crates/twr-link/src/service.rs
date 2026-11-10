//! One machine service, private local controls, bounded public admission, durable pairing.
use crate::{
    identity::Certificate,
    network,
    pairing::{Code, Derived, Transcript, Window},
    state::{Machine, Owner, Runner, State, Worker},
    wire,
};
use anyhow::{ensure, Context, Result};
use iroh::Endpoint;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
};
use twr_core::{
    paths::{ensure_private_dir, Paths},
    sealed::HubKey,
};

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Manage {
        name: String,
        workspace: String,
        admin: Admin,
    },
    Status,
    Prune {
        #[serde(default)]
        aggressive: bool,
    },
    Pressure {
        name: String,
        workspace: String,
        #[serde(default)]
        release: Option<String>,
    },
    SetCapacity {
        slots: u32,
    },
    SetAllowSleep {
        allow: bool,
    },
    PushRepo {
        name: String,
        workspace: String,
        project: String,
        repository: PathBuf,
        base: Option<String>,
    },
    PullRepo {
        name: String,
        workspace: String,
        project: String,
        repository: PathBuf,
        branch: String,
        result: String,
    },
    Open {
        name: String,
        workspace: String,
        executable: PathBuf,
        #[serde(default)]
        replace: bool,
    },
    Recover {
        name: String,
        workspace: String,
    },
    UseAsRunner,
    CancelPairing {
        session: Option<String>,
    },
    Connect {
        code: String,
        #[serde(default)]
        replace: bool,
        name: String,
        workspace: String,
    },
    Revoke {
        machine: String,
    },
    Forget {
        name: String,
    },
    SetEnabled {
        enabled: bool,
    },
    Stop,
}
#[derive(Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    Ok(Value),
    Error(String),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Admin {
    Status,
    Invite,
    Cancel { session: Option<String> },
    Revoke { machine: String },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum RemoteRequest {
    Retire {
        certificate: Certificate,
        receipt: String,
    },
    Authenticate {
        certificate: Certificate,
        receipt: String,
    },
    Admin {
        certificate: Certificate,
        admin: Admin,
    },
    Pair {
        session: String,
        admission: String,
        transcript: Transcript,
        proof: String,
        signature: String,
    },
    Commit {
        certificate: Certificate,
        receipt: String,
    },
    Pressure {
        certificate: Certificate,
        #[serde(default)]
        release: Option<String>,
    },
    CheckUpdate {
        certificate: Certificate,
    },
    Probe {
        certificate: Certificate,
    },
    BuildState {
        certificate: Certificate,
        source: crate::source::Source,
    },
    BuildSource {
        certificate: Certificate,
        source: crate::source::Source,
    },
    Have {
        certificate: Certificate,
        name: String,
        project: String,
    },
    Import {
        certificate: Certificate,
        name: String,
        project: String,
        bundle: crate::bundle::Bundle,
    },
    Export {
        certificate: Certificate,
        name: String,
        project: String,
        branch: String,
    },
    Install {
        certificate: Certificate,
        build: crate::worker::Build,
    },
    Open {
        certificate: Certificate,
        name: String,
        build: crate::worker::Build,
        features: Vec<String>,
        #[serde(default)]
        replace: bool,
    },
}
impl RemoteRequest {
    fn certificate(&self) -> &Certificate {
        match self {
            Self::Pair { transcript, .. } => &transcript.certificate,
            Self::Retire { certificate, .. }
            | Self::Authenticate { certificate, .. }
            | Self::Admin { certificate, .. }
            | Self::Pressure { certificate, .. }
            | Self::CheckUpdate { certificate }
            | Self::Probe { certificate }
            | Self::BuildState { certificate, .. }
            | Self::BuildSource { certificate, .. }
            | Self::Have { certificate, .. }
            | Self::Import { certificate, .. }
            | Self::Export { certificate, .. }
            | Self::Install { certificate, .. }
            | Self::Open { certificate, .. }
            | Self::Commit { certificate, .. } => certificate,
        }
    }
}
struct ConnectionGuard(iroh::endpoint::Connection);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"link connection ended");
    }
}
const FEATURES: &[&str] = &[
    "owner_auth_v1",
    "managed_paths_v1",
    "global_leases_v1",
    "revocation_v1",
    "bulk_v1",
    "capacity_v1",
    "git_bundle_v1",
    "source_build_v1",
    "worker_events_v7",
    "spool_v1",
];

enum BuildJob {
    Running,
    Failed(String, std::time::Instant),
}

struct PairWindow {
    window: Window,
    derived: Derived,
}
struct Workspace {
    endpoint: Endpoint,
    certificate: Certificate,
}
type PublishFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
type Publisher = Arc<dyn Fn(pkarr::SignedPacket) -> PublishFuture + Send + Sync>;
type ControlConnection = (String, Arc<Workspace>, iroh::endpoint::Connection);
pub type PeerGuard = Arc<dyn Fn(u32) -> bool + Send + Sync>;

struct Service {
    publisher: Publisher,
    peer_guard: PeerGuard,
    unauth: Arc<Semaphore>,
    kdf: Semaphore,
    bulk: Semaphore,
    update_lock: Mutex<()>,
    retirement_lock: Mutex<()>,
    retirement_until: Mutex<(std::time::Instant, Option<String>)>,
    controls: Mutex<HashMap<String, ControlConnection>>,
    pairing_control: Mutex<()>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    shutting: std::sync::atomic::AtomicBool,
    update_requested: std::sync::atomic::AtomicBool,
    upgrade: Mutex<Option<PathBuf>>,
    failed: std::sync::atomic::AtomicBool,
    sessions: Mutex<HashMap<String, HashMap<usize, iroh::endpoint::Connection>>>,
    outgoing: Mutex<HashMap<String, Vec<iroh::endpoint::Connection>>>,
    builds: Mutex<HashMap<String, BuildJob>>,
    machine: Machine,
    paths: Paths,
    state: Mutex<State>,
    identity: HubKey,
    runner: Mutex<Option<Endpoint>>,
    workspaces: Mutex<HashMap<String, Arc<Workspace>>>,
    window: Mutex<Option<PairWindow>>,
    stopping: tokio::sync::Notify,
}

/// Foreground service entry point. The lifetime flock serializes state and identity creation.
/// How often the service checks that its socket is still the one it listens on.
const SOCKET_CHECK: Duration = Duration::from_secs(30);
/// The device and inode of the socket at `path`, if one is there.
fn socket_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_socket())
        .map(|m| (m.dev(), m.ino()))
}
pub async fn run(root: PathBuf, socket: PathBuf, peer_guard: PeerGuard) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    let machine = Machine {
        dir: root.join("machine"),
        socket,
    };
    machine.init()?;
    let lock = twr_core::paths::open_private_append(&machine.dir.join("service.lock"))?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "runner service is already running"
    );
    // The lifetime lock proves any old socket belongs to a service that ended.
    match std::fs::symlink_metadata(&machine.socket) {
        Ok(m) => {
            use std::os::unix::fs::FileTypeExt;
            ensure!(m.file_type().is_socket(), "runner socket is not a socket");
            std::fs::remove_file(&machine.socket)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&machine.socket)?;
    twr_core::paths::restrict_file(&machine.socket)?;
    let bound = socket_identity(&machine.socket);
    ensure_private_dir(&machine.dir.join("identity"))?;
    let identity = HubKey::load_or_create(&machine.dir.join("identity/link-key"))?;
    let mut state = machine.load()?;
    machine.reconcile(&mut state)?;
    let saved = state.owners.values().any(|o| !o.revoked);
    let paths = Paths {
        data_dir: root,
        socket: machine.socket.with_file_name("hub.sock"),
        config_file: PathBuf::new(),
        workspace: None,
    };
    let service = Arc::new(Service {
        publisher: Arc::new(|packet| Box::pin(network::publish(packet))),
        peer_guard,
        unauth: Arc::new(Semaphore::new(16)),
        kdf: Semaphore::new(1),
        bulk: Semaphore::new(1),
        update_lock: Mutex::new(()),
        retirement_lock: Mutex::new(()),
        retirement_until: Mutex::new((std::time::Instant::now(), None)),
        controls: Mutex::new(HashMap::new()),
        pairing_control: Mutex::new(()),
        tasks: Default::default(),
        shutting: Default::default(),
        update_requested: Default::default(),
        upgrade: Mutex::new(None),
        failed: Default::default(),
        sessions: Default::default(),
        outgoing: Default::default(),
        builds: Default::default(),
        machine: machine.clone(),
        paths,
        state: Mutex::new(state),
        identity,
        runner: Mutex::new(None),
        workspaces: Mutex::new(HashMap::new()),
        window: Mutex::new(None),
        stopping: Default::default(),
    });
    {
        let machine = machine.clone();
        service.spawn(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let runtimes =
                    twr_runner::managed::Managed::retire_fenced(&machine.dir.join("runner/leases"));
                let builds = crate::source::retire_orphan(&machine);
                runtimes?;
                builds?;
                Ok(())
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!(
                    "orphan runtime/build cleanup could not finish; inspect runner diagnostics"
                );
            }
        });
    }
    if saved {
        let s = service.clone();
        service.spawn(async move {
            if let Err(e) = s.endpoint().await {
                tracing::warn!("runner links unavailable: {e:#}");
            }
        });
    }
    let slots = Arc::new(Semaphore::new(32));
    let mut maintenance = tokio::time::interval(Duration::from_secs(1));
    let mut next_update = std::time::Instant::now() + Duration::from_secs(6 * 60 * 60);
    let mut next_prune = std::time::Instant::now() + Duration::from_secs(60);
    let mut next_retirement = std::time::Instant::now();
    let mut next_socket_check = std::time::Instant::now() + SOCKET_CHECK;
    let mut power = crate::power::Power::default();
    let shutdown = twr_runner::client::shutdown_signal();
    tokio::pin!(shutdown);
    // The twr home this service serves, and the machine state in it: when either has been
    // gone long enough (deleted without `twr runner stop`), nothing can use this service
    // again, and it ends (`twr_runner::gone`).
    let anchors: Arc<Vec<twr_runner::gone::Anchor>> = Arc::new(
        [service.paths.data_dir.as_path(), machine.dir.as_path()]
            .iter()
            .filter_map(|p| twr_runner::gone::Anchor::of(p).ok())
            .collect(),
    );
    let home_gone = home_gone(anchors.clone());
    tokio::pin!(home_gone);
    let mut gone = false;
    loop {
        tokio::select! {
            _=service.stopping.notified()=>break,
            ()=&mut home_gone=>{
                tracing::warn!("the twr home this runner service serves is gone: stopping, and taking its launchd job away");
                gone=true;
                break;
            },
            _=maintenance.tick()=>{
                // A runtime directory removed at logout takes the socket with it: no client
                // could reach this service, and its lifetime lock would keep a new one from
                // starting. Its workers run on; the next client starts a service again.
                if std::time::Instant::now()>=next_socket_check {
                    next_socket_check=std::time::Instant::now()+SOCKET_CHECK;
                    if bound.is_some() && socket_identity(&machine.socket)!=bound {
                        tracing::warn!("the runner service socket was removed; stopping so a client can start the service again");
                        break;
                    }
                }
                service.expire_pairing().await;
                service.retire_workspaces().await;
                if std::time::Instant::now()>=next_prune {next_prune=std::time::Instant::now()+Duration::from_secs(6*60*60);let s=service.clone();let a=anchors.clone();service.spawn(async move {if home_there(&a).await {let _=s.control(Request::Prune{aggressive:false}).await;}});}
                if std::time::Instant::now()>=next_retirement {
                    next_retirement=std::time::Instant::now()+Duration::from_secs(60);
                    let s=service.clone(); let a=anchors.clone(); service.spawn(async move { if home_there(&a).await { s.retry_retirements().await; } });
                }
                // (Its lease files are made where they are missing: not in a home that is gone,
                // nor on a volume's mount point while it is not mounted.)
                if anchors.iter().all(|a| twr_runner::gone::look(a)==twr_runner::gone::Seen::There) {
                    power.update(&machine.dir.join("runner/leases"),service.state.lock().await.allow_sleep);
                }
                let requested=service.update_requested.swap(false,std::sync::atomic::Ordering::AcqRel);
                if requested || std::time::Instant::now()>=next_update {
                    next_update=std::time::Instant::now()+Duration::from_secs(6*60*60);
                    if service.state.lock().await.owners.values().any(|o|o.committed&&!o.revoked){let s=service.clone();let a=anchors.clone();service.spawn(async move {if home_there(&a).await {let _=s.check_update().await;}});}
                }
            },
            _=&mut shutdown=>break,
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;
                let Ok(permit)=slots.clone().try_acquire_owned() else {drop(stream);continue};
                let s=service.clone();service.spawn(async move {let _permit=permit;let _=s.local(stream).await;});
            }
        }
    }
    service
        .shutting
        .store(true, std::sync::atomic::Ordering::Release);
    let tasks = std::mem::take(&mut *service.tasks.lock().unwrap_or_else(|e| e.into_inner()));
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
    let workspaces = std::mem::take(&mut *service.workspaces.lock().await);
    for workspace in workspaces.into_values() {
        workspace.endpoint.close().await;
    }
    if let Some(endpoint) = service.runner.lock().await.take() {
        endpoint.close().await;
    }
    let _ = std::fs::remove_file(&machine.socket);
    drop(lock);
    if gone {
        // No update for a home that is gone; nothing restarts this service either.
        twr_runner::gone::unregister_own_job();
        return Ok(());
    }
    if let Some(candidate) = service.upgrade.lock().await.take() {
        // Worker daemons are independent. Maintenance closes service streams, then the
        // bootstrapper performs readiness checks and rolls back to this executable on failure.
        if let Err(e) = crate::bootstrap::replace(&service.paths, &candidate).await {
            tracing::warn!("runner service update failed: {e:#}");
            let _ = crate::bootstrap::replace(&service.paths, &std::env::current_exe()?).await;
        }
    }
    Ok(())
}
/// Resolves once one of `anchors` (the twr home a service serves, and its machine state) has
/// been gone long enough ([`twr_runner::gone`]); never when they could not both be anchored at
/// start.
async fn home_gone(anchors: Arc<Vec<twr_runner::gone::Anchor>>) {
    use twr_runner::gone::until_gone;
    let [root, machine] = anchors.as_slice() else {
        return std::future::pending().await;
    };
    tokio::select! {
        () = until_gone(root.clone()) => {}
        () = until_gone(machine.clone()) => {}
    }
}
/// Whether every one of `anchors` is there now: the periodic upkeep that writes in the home
/// (prunes, retirements) runs only then, so a home that is gone is not made again, nor a
/// volume's mount point written to while it is not mounted.
async fn home_there(anchors: &[twr_runner::gone::Anchor]) -> bool {
    for a in anchors {
        if twr_runner::gone::look_bounded(a).await != twr_runner::gone::Seen::There {
            return false;
        }
    }
    true
}
impl Service {
    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if self.shutting.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio::spawn(future));
    }
    fn active(&self) -> Result<()> {
        ensure!(
            !self.failed.load(std::sync::atomic::Ordering::Acquire)
                && !self.shutting.load(std::sync::atomic::Ordering::Acquire),
            "runner service is stopping or its state could not be saved"
        );
        Ok(())
    }
    async fn persist(&self, state: &mut State) -> Result<()> {
        if let Err(e) = self.machine.save(state) {
            // No more authority is admitted after a durability failure. Restart reloads disk.
            self.failed
                .store(true, std::sync::atomic::Ordering::Release);
            self.stopping.notify_one();
            return Err(e);
        }
        Ok(())
    }
    async fn check_update(self: &Arc<Self>) -> Result<()> {
        let Ok(_single) = self.update_lock.try_lock() else {
            return Ok(());
        };
        let slot = self.machine.dir.join("service/releases");
        ensure_private_dir(&slot)?;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args(["runner", "fetch-service", "--slot"])
            .arg(&slot)
            .env_remove("TWR_WORKSPACE");
        let output =
            twr_adapters::process::run_bounded(command, Duration::from_secs(15 * 60), 4096, |_| {
                false
            })
            .await;
        if let twr_adapters::process::Bounded::Exited(Some(0), bytes) = output {
            let value: Value = serde_json::from_slice(&bytes)?;
            if value["updated"] == true {
                let candidate = PathBuf::from(
                    value["executable"]
                        .as_str()
                        .context("service candidate missing")?,
                );
                ensure!(
                    candidate == slot.join("twr"),
                    "unexpected service candidate path"
                );
                *self.upgrade.lock().await = Some(candidate);
                self.stopping.notify_one();
            }
        }
        Ok(())
    }
    async fn endpoint(self: &Arc<Self>) -> Result<Endpoint> {
        let mut current = self.runner.lock().await;
        if let Some(endpoint) = current.as_ref() {
            return Ok(endpoint.clone());
        }
        let secret = crate::state::secret(&self.machine.dir.join("identity/runner-key"))?;
        let endpoint = network::endpoint(secret).await?;
        let accept = endpoint.clone();
        let service = self.clone();
        self.spawn(async move {
            let slots = Arc::new(Semaphore::new(16));
            while let Some(incoming) = accept.accept().await {
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                let receiver = service.clone();
                service.spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(connection)) =
                        tokio::time::timeout(Duration::from_secs(10), incoming).await
                    {
                        drop(_permit);
                        let _ = receiver.remote(connection).await;
                    }
                });
            }
        });
        *current = Some(endpoint.clone());
        Ok(endpoint)
    }
    async fn retire_if_unused(&self) {
        let window = self.window.lock().await;
        let state = self.state.lock().await;
        if state.owners.values().any(|o| !o.revoked) || window.is_some() {
            return;
        }
        let endpoint = self.runner.lock().await.take();
        drop(state);
        drop(window);
        if let Some(endpoint) = endpoint {
            endpoint.close().await;
        }
    }
    async fn publish_window(
        self: &Arc<Self>,
        display: String,
        derived: Derived,
        window: Window,
        record: crate::locator::Record,
    ) -> Result<Value> {
        let packet = record.sign(&derived.locator_key())?;
        *self.window.lock().await = Some(PairWindow { window, derived });
        let publication =
            tokio::time::timeout(Duration::from_secs(30), (self.publisher)(packet)).await;
        if !matches!(publication, Ok(Ok(()))) {
            self.cancel_pairing(Some(&record.session)).await?;
            anyhow::bail!("could not publish pairing code; try again");
        }
        Ok(
            json!({"code":display,"expires_unix":record.expires_unix,"session":record.session,"allow_sleep":self.state.lock().await.allow_sleep,"instruction":"On the other hub, choose Connect a runner or run twr box pair <name>."}),
        )
    }
    async fn cancel_pairing(&self, session: Option<&str>) -> Result<()> {
        let mut window = self.window.lock().await;
        if session
            .is_some_and(|session| window.as_ref().is_some_and(|p| p.window.session != session))
        {
            return Ok(());
        }
        let pair = window.take();
        drop(window);
        // The local retirement is authoritative even when tombstone publication fails.
        if let Some(pair) = pair {
            if let Ok(packet) = pkarr::SignedPacket::builder().sign(&pair.derived.locator_key()) {
                let publisher = self.publisher.clone();
                self.spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(10), publisher(packet)).await;
                });
            }
        }
        self.retire_if_unused().await;
        Ok(())
    }
    async fn expire_pairing(&self) {
        let expired = self.window.lock().await.as_ref().is_some_and(|p| {
            p.window
                .admit(&p.window.session, &p.window.admission)
                .is_err()
        });
        if expired {
            let _ = self.cancel_pairing(None).await;
        }
    }
    async fn retire_workspaces(&self) {
        // Renames preserve the UID/generation and endpoint; deletion/recreation cannot reuse
        // a cached endpoint just because its human-readable workspace name is unchanged.
        let mut names = vec!["default".to_string()];
        if let Ok(named) = self.paths.named_workspaces() {
            names.extend(named);
        } else {
            return;
        }
        let mut live = std::collections::HashSet::new();
        for name in names {
            if let Ok(paths) = self.paths.workspace(&name) {
                if let Ok(bytes) =
                    crate::state::read_private(&paths.data_dir.join("link-identity"), 64)
                {
                    if bytes.len() == 64 {
                        live.insert(format!(
                            "{}/{}-{}",
                            self.identity.public_hex(),
                            hex::encode(&bytes[..16]),
                            hex::encode(&bytes[16..32])
                        ));
                    }
                }
            }
        }
        let mut cached = self.workspaces.lock().await;
        let stale: Vec<_> = cached
            .iter()
            .filter(|(_, w)| !live.contains(&w.certificate.scope()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            if let Some(w) = cached.remove(&key) {
                w.endpoint.close().await;
            }
        }
    }
    async fn workspace(&self, name: &str) -> Result<Arc<Workspace>> {
        let paths = self.paths.workspace(name)?;
        ensure!(paths.exists(), "workspace does not exist");
        let mut workspaces = self.workspaces.lock().await;
        let file = paths.data_dir.join("link-identity");
        let bytes = match crate::state::read_private(&file, 64) {
            Ok(b) => b,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                let b = crate::random::<64>()?;
                crate::state::atomic_write(&file, &b)?;
                b.to_vec()
            }
            Err(e) => return Err(e),
        };
        ensure!(bytes.len() == 64, "invalid workspace link identity");
        let key = HubKey::load_or_create(&paths.data_dir.join("box-key"))?;
        for w in workspaces.values() {
            if w.certificate.binding.workspace == hex::encode(&bytes[..16])
                && w.certificate.binding.generation == hex::encode(&bytes[16..32])
                && w.certificate.binding.workspace_key == key.public_hex()
                && w.endpoint.id() == iroh::SecretKey::from_bytes(&bytes[32..].try_into()?).public()
            {
                return Ok(w.clone());
            }
        }
        let endpoint = network::endpoint(bytes[32..].try_into()?).await?;
        let certificate = Certificate::issue(
            &self.identity,
            &key,
            endpoint.id(),
            hex::encode(&bytes[..16]),
            hex::encode(&bytes[16..32]),
        )?;
        let w = Arc::new(Workspace {
            endpoint,
            certificate,
        });
        workspaces.insert(w.certificate.scope(), w.clone());
        Ok(w)
    }
    async fn local(self: Arc<Self>, mut stream: UnixStream) -> Result<()> {
        let creds = stream.peer_cred()?;
        ensure!(
            creds.uid() == twr_core::paths::effective_uid(),
            "runner controls require the owning user"
        );
        let pid = creds.pid().context("cannot identify runner control peer")? as u32;
        let guard = self.peer_guard.clone();
        let human = tokio::task::spawn_blocking(move || guard(pid)).await?;
        ensure!(human, "runner controls are user-only");
        let request: Request = wire::read(&mut stream).await?;
        self.active()?;
        let stop = matches!(request, Request::Stop);
        if let Request::Open {
            name,
            workspace,
            executable,
            replace,
        } = request
        {
            match self.open(&name, &workspace, &executable, replace).await {
                Ok((mut remote, root, build)) => {
                    wire::write(&mut stream, &Reply::Ok(json!({"root":root,"build":build})))
                        .await?;
                    tokio::io::copy_bidirectional(&mut stream, &mut remote).await?;
                }
                Err(e) => wire::write(&mut stream, &Reply::Error(format!("{e:#}"))).await?,
            }
            return Ok(());
        }
        let response = match self.control(request).await {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Error(format!("{e:#}")),
        };
        wire::write(&mut stream, &response).await?;
        if stop {
            self.stopping.notify_one();
        }
        Ok(())
    }
    fn control(
        self: &Arc<Self>,
        request: Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + '_>> {
        Box::pin(async move {
            ensure!(
                !self.failed.load(std::sync::atomic::Ordering::Acquire),
                "service state could not be saved"
            );
            match request {
                Request::Manage {
                    name,
                    workspace,
                    admin,
                } => {
                    let (w, c, _) = self.linked_control(&name, &workspace).await?;
                    self.remote_request(
                        &c,
                        &RemoteRequest::Admin {
                            certificate: w.certificate.clone(),
                            admin,
                        },
                    )
                    .await
                }
                Request::Pressure {
                    name,
                    workspace,
                    release,
                } => {
                    let (w, c, _) = self.linked_control(&name, &workspace).await?;
                    self.remote_request(
                        &c,
                        &RemoteRequest::Pressure {
                            certificate: w.certificate.clone(),
                            release,
                        },
                    )
                    .await
                }
                Request::SetCapacity { slots } => {
                    ensure!(
                        (1..=64).contains(&slots),
                        "runtime slots must be between 1 and 64"
                    );
                    crate::state::atomic_write(
                        &self.machine.dir.join("runner/leases/slots.json"),
                        &serde_json::to_vec(&slots)?,
                    )?;
                    Ok(json!({"runtime_slots":slots}))
                }
                Request::SetAllowSleep { allow } => {
                    let mut state = self.state.lock().await;
                    state.allow_sleep = allow;
                    self.persist(&mut state).await?;
                    Ok(json!({"allow_sleep":allow}))
                }
                Request::PushRepo {
                    name,
                    workspace,
                    project,
                    repository,
                    base,
                } => {
                    self.push_repo(&name, &workspace, &project, &repository, base.as_deref())
                        .await
                }
                Request::PullRepo {
                    name,
                    workspace,
                    project,
                    repository,
                    branch,
                    result,
                } => {
                    self.pull_repo(&name, &workspace, &project, &repository, &branch, &result)
                        .await
                }
                Request::Open { .. } => unreachable!(),
                Request::Recover { name, workspace } => {
                    self.recover(&name, &workspace).await?;
                    Ok(json!({"name":name,"paired":true}))
                }
                Request::Prune { aggressive } => {
                    let _bulk = self.bulk.acquire().await?;
                    let state = self.state.lock().await.clone();
                    Ok(
                        json!({"reclaimed_bytes":crate::cache::prune(&self.machine,&state,aggressive)?}),
                    )
                }
                Request::Status => {
                    let state = self.state.lock().await;
                    Ok(
                        json!({"running":true,"service_protocol":1,"state_schema":1,"runtime_slots":twr_runner::managed::Managed::slots(&self.machine.dir.join("runner/leases"))?,"pending_revocations":state.pending_revocations.len(),"build":twr_core::build_id(),"executable":std::env::current_exe().ok(),"enabled":!state.disabled,"allow_sleep":state.allow_sleep,"launch":twr_runner::daemon::own_launch(),"shared_user":true,"machine":self.identity.public_hex(),"endpoint":self.runner.lock().await.as_ref().map(|e|e.id().to_string()),"runners":state.runners.iter().map(|(name,r)|json!({"name":name,"endpoint":r.endpoint,"paired":r.committed})).collect::<Vec<_>>(),"hubs":state.owners.iter().map(|(key,o)|json!({"machine":key,"paired":o.committed,"revoked":o.revoked})).collect::<Vec<_>>()}),
                    )
                }
                Request::UseAsRunner => {
                    let _single = self.pairing_control.lock().await;
                    let result = async {
                        let _kdf = self.kdf.acquire().await?;
                        let code = Code::generate()?;
                        let derived = tokio::task::spawn_blocking(move || {
                            let display = code.display();
                            Ok::<_, anyhow::Error>((display, code.derive()?))
                        })
                        .await??;
                        drop(_kdf);
                        let endpoint = self.endpoint().await?;
                        tokio::time::timeout(Duration::from_secs(20), endpoint.online()).await?;
                        let relay = endpoint
                            .addr()
                            .relay_urls()
                            .next()
                            .context("runner relay is unavailable")?
                            .to_string();
                        let window = Window::new()?;
                        let record = crate::locator::Record {
                            version: 1,
                            endpoint: endpoint.id().to_string(),
                            relay,
                            expires_unix: window.expires,
                            session: window.session.clone(),
                            admission: window.admission.clone(),
                        };
                        self.publish_window(derived.0, derived.1, window, record)
                            .await
                    }
                    .await;
                    if result.is_err() {
                        self.retire_if_unused().await;
                    }
                    result
                }

                Request::CancelPairing { session } => {
                    let _single = self.pairing_control.lock().await;
                    self.cancel_pairing(session.as_deref()).await?;
                    Ok(json!({"cancelled":true}))
                }
                Request::Connect {
                    code,
                    replace,
                    name,
                    workspace,
                } => {
                    self.connect(Code::parse(&code)?, name, workspace, replace)
                        .await
                }
                Request::Revoke { machine } => self.revoke_machine(&machine, None).await,
                Request::Forget { name } => {
                    let mut state = self.state.lock().await;
                    let Some(runner) = state.runners.remove(&name) else {
                        drop(state);
                        self.retry_retirements().await;
                        return Ok(
                            json!({"forgotten":true,"pending_acknowledgement":!self.state.lock().await.pending_revocations.is_empty()}),
                        );
                    };
                    let id = format!("{}-{}", runner.endpoint, runner.receipt);
                    if !state
                        .runners
                        .values()
                        .any(|r| r.endpoint == runner.endpoint)
                    {
                        state.pending_revocations.insert(id, runner);
                    }
                    self.persist(&mut state).await?;
                    if let Some(connections) = self.outgoing.lock().await.remove(&name) {
                        for c in connections {
                            c.close(2u32.into(), b"runner forgotten");
                        }
                    }
                    drop(state);
                    self.retry_retirements().await;
                    Ok(
                        json!({"forgotten":true,"pending_acknowledgement":!self.state.lock().await.pending_revocations.is_empty()}),
                    )
                }
                Request::SetEnabled { enabled } => {
                    let mut state = self.state.lock().await;
                    state.disabled = !enabled;
                    self.persist(&mut state).await?;
                    Ok(json!({"enabled":enabled}))
                }
                Request::Stop => Ok(json!({"stopping":true})),
            }
        })
    }
    fn retire_processes(&self, owner: &str, retired_epoch: u64) {
        let owner_key = owner.to_string();
        let runner_machine = self.machine.clone();
        self.spawn(async move {
            let result=tokio::task::spawn_blocking(move||->Result<()>{
                let runtimes=twr_runner::managed::Managed::retire_grant(&runner_machine.dir.join("runner/leases"),&runner_machine.fence(&owner_key,retired_epoch));
                let builds=crate::source::retire_grant(&runner_machine,&owner_key,retired_epoch);
                runtimes?;builds?;Ok(())
            }).await;
            if !matches!(result,Ok(Ok(()))){tracing::warn!("some revoked runtime/build processes could not be retired; inspect runner diagnostics");}
        });
    }
    async fn revoke_machine(&self, machine: &str, keep: Option<usize>) -> Result<Value> {
        let mut state = self.state.lock().await;
        let owner = state
            .owners
            .get_mut(machine)
            .context("paired hub not found")?;
        let retired_epoch = owner.epoch;
        self.machine.retire(machine, retired_epoch)?;
        owner.revoked = true;
        owner.epoch = owner
            .epoch
            .checked_add(1)
            .context("revocation epoch exhausted")?;
        self.persist(&mut state).await?;
        if let Some(sessions) = self.sessions.lock().await.remove(machine) {
            for connection in sessions.into_values() {
                if keep != Some(connection.stable_id()) {
                    connection.close(2u32.into(), b"hub revoked");
                }
            }
        }
        let workers: Vec<_> = state
            .workers
            .values()
            .filter(|w| w.machine == machine)
            .cloned()
            .collect();
        drop(state);
        // Fences are durable before any stop. Workers also observe these without us.
        self.retire_processes(machine, retired_epoch);
        let machine = self.machine.clone();
        for worker in workers {
            let machine = machine.clone();
            self.spawn(async move {
                let _ = crate::worker::stop(&machine, &worker).await;
            });
        }
        if keep.is_none() {
            self.retire_if_unused().await;
        }
        Ok(json!({"revoked":true}))
    }
    async fn connect(
        self: &Arc<Self>,
        code: Code,
        name: String,
        workspace: String,
        replace: bool,
    ) -> Result<Value> {
        twr_core::runner::validate_box_name(&name)?;
        ensure!(
            !self.state.lock().await.runners.contains_key(&name),
            "runner name already exists"
        );
        let kdf = self.kdf.acquire().await?;
        let derived = tokio::task::spawn_blocking(move || code.derive()).await??;
        drop(kdf);
        let packet = tokio::time::timeout(
            Duration::from_secs(30),
            network::locator_client()?.resolve(
                &derived.locator_key().public_key(),
                pkarr::ResolvePolicy::CacheFirst,
            ),
        )
        .await??;
        let record = crate::locator::Record::parse(
            &packet,
            &derived.locator_key().public_key(),
            crate::now(),
        )?;
        self.connect_record(derived, record, name, workspace, replace)
            .await
    }
    async fn connect_record(
        &self,
        derived: Derived,
        record: crate::locator::Record,
        name: String,
        workspace: String,
        replace: bool,
    ) -> Result<Value> {
        let workspace = self.workspace(&workspace).await?;
        let mut connection = tokio::time::timeout(
            Duration::from_secs(30),
            workspace.endpoint.connect(record.address()?, crate::ALPN),
        )
        .await??;
        // Machine trust is shared by its workspaces and aliases. Reuse a live grant rather
        // than rotating it and interrupting workers just to add another local name.
        let existing = self
            .state
            .lock()
            .await
            .runners
            .values()
            .find(|r| r.endpoint == record.endpoint)
            .cloned();
        if let Some(mut runner) = existing {
            if self
                .remote_request(
                    &connection,
                    &RemoteRequest::Authenticate {
                        certificate: workspace.certificate.clone(),
                        receipt: runner.receipt.clone(),
                    },
                )
                .await
                .is_ok()
            {
                self.commit(&connection, &workspace.certificate, &runner.receipt)
                    .await?;
                runner.committed = true;
                {
                    let mut state = self.state.lock().await;
                    ensure!(
                        !state.runners.contains_key(&name),
                        "runner name was paired concurrently"
                    );
                    state.runners.insert(name.clone(), runner);
                    self.persist(&mut state).await?;
                }
                let _ = self
                    .remote_request(
                        &connection,
                        &RemoteRequest::Admin {
                            certificate: workspace.certificate.clone(),
                            admin: Admin::Cancel {
                                session: Some(record.session.clone()),
                            },
                        },
                    )
                    .await;
                connection.close(0u32.into(), b"existing machine grant reused");
                return Ok(json!({"name":name,"paired":true,"endpoint":record.endpoint}));
            }
            connection.close(0u32.into(), b"replacing retired grant");
            connection = tokio::time::timeout(
                Duration::from_secs(30),
                workspace.endpoint.connect(record.address()?, crate::ALPN),
            )
            .await??;
        }
        let exporter = network::exporter(&connection)?;
        let transcript = Transcript {
            version: 1,
            replace,
            hub: workspace.endpoint.id().to_string(),
            runner: connection.remote_id().to_string(),
            locator: derived.locator_key().public_key().to_string(),
            session: record.session.clone(),
            expires: record.expires_unix,
            certificate: workspace.certificate.clone(),
        };
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut signed = serde_json::to_vec(&transcript)?;
        signed.extend(exporter);
        wire::write(
            &mut send,
            &RemoteRequest::Pair {
                session: record.session,
                admission: record.admission,
                proof: transcript.proof(&derived, &exporter, "hub")?,
                signature: hex::encode(self.identity.sign(&signed)),
                transcript: transcript.clone(),
            },
        )
        .await?;
        let reply: Reply = wire::read(&mut recv).await?;
        let Reply::Ok(value) = reply else {
            anyhow::bail!("runner refused pairing")
        };
        transcript.verify(
            &derived,
            &exporter,
            "runner",
            value["proof"]
                .as_str()
                .context("runner confirmation missing")?,
        )?;
        let receipt = value["receipt"]
            .as_str()
            .context("pairing receipt missing")?
            .to_string();
        {
            let mut state = self.state.lock().await;
            ensure!(
                !state.runners.contains_key(&name),
                "runner name was paired concurrently"
            );
            for old in state
                .runners
                .values_mut()
                .filter(|r| r.endpoint == connection.remote_id().to_string())
            {
                old.receipt = receipt.clone();
                old.committed = false;
            }
            state.runners.insert(
                name.clone(),
                Runner {
                    addresses: serde_json::from_value(
                        value.get("addresses").cloned().unwrap_or_else(|| json!([])),
                    )?,
                    endpoint: connection.remote_id().to_string(),
                    relay: record.relay,
                    receipt: receipt.clone(),
                    committed: false,
                },
            );
            self.persist(&mut state).await?;
        }
        self.commit(&connection, &workspace.certificate, &receipt)
            .await?;
        let mut state = self.state.lock().await;
        ensure!(
            state
                .runners
                .get(&name)
                .is_some_and(|r| r.receipt == receipt),
            "runner removed or replaced during pairing"
        );
        for runner in state
            .runners
            .values_mut()
            .filter(|r| r.endpoint == connection.remote_id().to_string() && r.receipt == receipt)
        {
            runner.committed = true;
        }
        self.persist(&mut state).await?;
        connection.close(0u32.into(), b"paired");
        Ok(json!({"name":name,"paired":true,"endpoint":connection.remote_id().to_string()}))
    }
    async fn commit(
        &self,
        connection: &iroh::endpoint::Connection,
        certificate: &Certificate,
        receipt: &str,
    ) -> Result<()> {
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::write(
            &mut send,
            &RemoteRequest::Commit {
                certificate: certificate.clone(),
                receipt: receipt.into(),
            },
        )
        .await?;
        ensure!(
            matches!(wire::read::<Reply>(&mut recv).await?, Reply::Ok(_)),
            "runner did not commit trust; retry recovery"
        );
        Ok(())
    }
    async fn remote(self: Arc<Self>, c: iroh::endpoint::Connection) -> Result<()> {
        let _close = ConnectionGuard(c.clone());
        let admission = self
            .unauth
            .clone()
            .try_acquire_owned()
            .context("too many unauthenticated peers")?;
        let (mut send, mut recv) =
            tokio::time::timeout(Duration::from_secs(10), c.accept_bi()).await??;
        let request: RemoteRequest = wire::read(&mut recv).await?;
        let binding = request.certificate().binding.clone();
        request.certificate().verify(c.remote_id())?;
        if let RemoteRequest::Retire {
            certificate,
            receipt,
        } = &request
        {
            let matches = self
                .state
                .lock()
                .await
                .owners
                .get(&certificate.binding.machine)
                .is_some_and(|o| !o.revoked && crate::pairing::same_secret(&o.receipt, receipt));
            if matches {
                self.revoke_machine(&certificate.binding.machine, Some(c.stable_id()))
                    .await?;
            }
            wire::write(&mut send, &Reply::Ok(json!({"retired":true}))).await?;
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
            self.retire_if_unused().await;
            return Ok(());
        }
        let value = match request {
            RemoteRequest::Authenticate {
                certificate,
                receipt,
            } => {
                let state = self.state.lock().await;
                let owner = state
                    .owners
                    .get(&certificate.binding.machine)
                    .context("hub is not paired")?;
                ensure!(
                    !owner.revoked && crate::pairing::same_secret(&owner.receipt, &receipt),
                    "stale or revoked trust grant"
                );
                json!({"authenticated":true,"epoch":owner.epoch})
            }
            request @ (RemoteRequest::Pair { .. } | RemoteRequest::Commit { .. }) => {
                self.remote_control(&c, request, None).await?
            }
            _ => anyhow::bail!("authenticate the link before requesting operations"),
        };
        let epoch = value["epoch"].as_u64().context("trust epoch missing")?;
        wire::write(&mut send, &Reply::Ok(value)).await?;
        send.finish()?;
        drop(admission);
        let slots = Arc::new(Semaphore::new(8));
        loop {
            let accepted = tokio::time::timeout(Duration::from_secs(60), c.accept_bi()).await;
            let (send, mut recv) = match accepted {
                Ok(result) => result?,
                Err(_) if slots.available_permits() < 8 => continue,
                Err(e) => return Err(e.into()),
            };
            let request: RemoteRequest = wire::read(&mut recv).await?;
            ensure!(
                request.certificate().binding == binding,
                "connection workspace binding changed"
            );
            match &request {
                RemoteRequest::Pair { .. }
                | RemoteRequest::Retire { .. }
                | RemoteRequest::Authenticate { .. } => {
                    anyhow::bail!("link is already authenticated")
                }
                RemoteRequest::Commit { .. } => {}
                _ => {
                    self.register_session(request.certificate(), &c, epoch)
                        .await?
                }
            }
            let permit = slots
                .clone()
                .try_acquire_owned()
                .context("too many link streams")?;
            let service = self.clone();
            let connection = c.clone();
            self.spawn(async move {
                let _permit = permit;
                if service
                    .serve_request(connection.clone(), send, recv, request, epoch)
                    .await
                    .is_err()
                {
                    connection.close(1u32.into(), b"link request refused");
                }
            });
        }
    }
    async fn serve_request(
        self: Arc<Self>,
        c: iroh::endpoint::Connection,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
        request: RemoteRequest,
        epoch: u64,
    ) -> Result<()> {
        let s = self;
        match request {
            RemoteRequest::Import {
                certificate,
                name,
                project,
                bundle,
            } => {
                let _bulk = s.bulk.acquire().await?;
                bundle.validate()?;
                let (epoch, scope) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                let root = s.repo_path(&certificate, &scope, &name, &project)?;
                let hub_root = s
                    .machine
                    .dir
                    .join("runner/hubs")
                    .join(&certificate.binding.machine);
                crate::bundle::reserve(&hub_root, bundle.size).await?;
                let incoming = root.parent().unwrap().join("incoming.bundle");
                let file = crate::worker::Build {
                    sha256: bundle.sha256.clone(),
                    size: bundle.size,
                    os: String::new(),
                    arch: String::new(),
                    protocol: 0,
                };
                wire::write(&mut send, &Reply::Ok(json!({"ready":true}))).await?;
                let temp = crate::worker::receive(&mut recv, &incoming, &file).await?;
                ensure_private_dir(&root)?;
                let repo =
                    crate::bundle::Repository::init_format(&root, &bundle.commit, false).await?;
                repo.git(&["config", "gc.auto", "0"]).await?;
                // Unpack and verify away from the published source ref; revocation needn't wait for Git.
                let quarantine = format!(
                    "refs/twr/quarantine/{}",
                    hex::encode(crate::random::<16>()?)
                );
                repo.receive(temp.path(), &bundle, &quarantine).await?;
                // Revocation cannot publish a source ref after its durable fence.
                let state = s.state.lock().await;
                let owner = state
                    .owners
                    .get(&certificate.binding.machine)
                    .context("owner missing")?;
                let active = !owner.revoked && owner.epoch == epoch;
                if active {
                    repo.git(&["update-ref", "refs/twr/source", &bundle.commit])
                        .await?;
                }
                drop(state);
                repo.git(&["update-ref", "-d", &quarantine]).await?;
                ensure!(active, "revoked during repository transfer");
                wire::write(
                    &mut send,
                    &Reply::Ok(json!({"repo":root,"commit":bundle.commit})),
                )
                .await?;
                send.finish()?;
            }
            RemoteRequest::Export {
                certificate,
                name,
                project,
                branch,
            } => {
                let (epoch, scope) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                let root = s.repo_path(&certificate, &scope, &name, &project)?;
                let repo = crate::bundle::Repository(root);
                let commit = repo.resolve(&branch).await?;
                let directory = tempfile::tempdir_in(
                    s.machine
                        .dir
                        .join("runner/hubs")
                        .join(&certificate.binding.machine),
                )?;
                let path = directory.path().join("results.bundle");
                let bundle = repo.create_bundle(&commit, &path, &[]).await?;
                let (current, _) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                ensure!(current == epoch, "revoked during result export");
                wire::write(&mut send, &Reply::Ok(json!({"bundle":bundle}))).await?;
                let file = crate::worker::Build {
                    sha256: bundle.sha256,
                    size: bundle.size,
                    os: String::new(),
                    arch: String::new(),
                    protocol: 0,
                };
                crate::worker::send(&mut send, &path, &file).await?;
                send.finish()?;
            }
            RemoteRequest::BuildSource {
                certificate,
                source,
            } => {
                let _bulk = s.bulk.acquire().await?;
                source.validate()?;
                let (epoch, scope) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                let id = format!(
                    "{scope}/{epoch}/{}",
                    source.id(crate::source::host_target())?
                );
                let jobs = s.builds.lock().await;
                ensure!(
                    !matches!(jobs.get(&id), Some(BuildJob::Running)),
                    "native build is already running"
                );
                drop(jobs);
                let owner_root = s
                    .machine
                    .dir
                    .join("runner/hubs")
                    .join(&certificate.binding.machine);
                ensure_private_dir(&owner_root)?;
                crate::bundle::reserve(&owner_root, source.bundle.size).await?;
                let temporary = s
                    .machine
                    .dir
                    .join("runner/hubs")
                    .join(&scope)
                    .join("native-incoming/source.bundle");
                let file = crate::worker::Build {
                    sha256: source.bundle.sha256.clone(),
                    size: source.bundle.size,
                    os: String::new(),
                    arch: String::new(),
                    protocol: 0,
                };
                wire::write(&mut send, &Reply::Ok(json!({"ready":true}))).await?;
                let temp = crate::worker::receive(&mut recv, &temporary, &file).await?;
                s.builds.lock().await.insert(id.clone(), BuildJob::Running);
                let service = s.clone();
                let cert = certificate.clone();
                let peer = c.remote_id();
                s.spawn(async move {
                                let build=crate::source::build(&service.machine,&scope,&source,temp.path(),epoch);
                                tokio::pin!(build);
                                let result=async {
                                    let native=loop {tokio::select!{result=&mut build=>break result?,_=tokio::time::sleep(Duration::from_millis(250))=>{let (current,_)=service.authorize_at(&cert,peer,epoch).await?;ensure!(current==epoch,"hub revoked during native build");}}};
                                    let state=service.state.lock().await;
                                    let owner=state.owners.get(&cert.binding.machine).context("owner missing")?;
                                    ensure!(owner.committed&&!owner.revoked&&owner.epoch==epoch,"hub revoked during native build");
                                    let destination=native.build.path(&service.machine,&scope);
                                    if native.executable!=destination {
                                        ensure_private_dir(destination.parent().unwrap())?;
                                        let output=tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
                                        std::fs::copy(native.executable,output.path())?;
                                        crate::worker::install(output,&destination)?;
                                    }
                                    crate::state::atomic_write(&native.output,&serde_json::to_vec(&native.build)?)?;
                                    // Authority was checked through publication. Scratch compiler output is
                                    // disposable; retaining the build lease prevents cleanup/install races.
                                    drop(state);
                                    if let Some(target)=&native.cleanup {if let Err(e)=std::fs::remove_dir_all(target){tracing::warn!("could not reclaim native compiler scratch output: {e}");}}Ok::<_,anyhow::Error>(())
                                }.await;
                                let mut jobs=service.builds.lock().await;
                                match result {Ok(())=>{jobs.remove(&id);},Err(e)=>{jobs.insert(id,BuildJob::Failed(format!("{e:#}"),std::time::Instant::now()));}}

                            });
                wire::write(&mut send, &Reply::Ok(json!({"building":true}))).await?;
                send.finish()?;
            }
            RemoteRequest::Install { certificate, build } => {
                let _bulk = s.bulk.acquire().await?;
                let (epoch, scope) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                build.validate()?;
                let path = build.path(&s.machine, &scope);
                if build.matches_async(&path).await {
                    wire::write(
                        &mut send,
                        &Reply::Ok(json!({"ready":false,"installed":true,"usable":crate::worker::Build::executable(&path).await.is_ok_and(|b|b.protocol==build.protocol)})),
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                let hub_root = s
                    .machine
                    .dir
                    .join("runner/hubs")
                    .join(&certificate.binding.machine);
                ensure_private_dir(&hub_root)?;
                crate::bundle::reserve(&hub_root, build.size).await?;
                wire::write(&mut send, &Reply::Ok(json!({"ready":true}))).await?;
                let temp = crate::worker::receive(&mut recv, &path, &build).await?;
                let state = s.state.lock().await;
                let owner = state
                    .owners
                    .get(&certificate.binding.machine)
                    .context("owner missing")?;
                ensure!(
                    owner.committed && !owner.revoked && owner.epoch == epoch,
                    "revoked during transfer"
                );
                crate::worker::install(temp, &path)?;
                drop(state);
                let usable = crate::worker::Build::executable(&path)
                    .await
                    .is_ok_and(|b| b.protocol == build.protocol);
                s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                wire::write(
                    &mut send,
                    &Reply::Ok(json!({"installed":true,"usable":usable})),
                )
                .await?;
                send.finish()?;
            }
            RemoteRequest::Open {
                certificate,
                name,
                build,
                features,
                replace,
            } => {
                twr_core::runner::validate_box_name(&name)?;
                for feature in FEATURES {
                    ensure!(
                        features.iter().any(|f| f == feature),
                        "mandatory linked-worker feature missing"
                    );
                }
                let (epoch, scope) = s.authorize_at(&certificate, c.remote_id(), epoch).await?;
                build.validate()?;
                let path = build.path(&s.machine, &scope);
                ensure!(
                    build.matches_async(&path).await,
                    "exact worker build is missing or changed"
                );
                let worker = Worker {
                    epoch,
                    machine: certificate.binding.machine.clone(),
                    scope: scope.clone(),
                    workspace_key: certificate.binding.workspace_key,
                    box_name: name.clone(),
                    build,
                };
                let opts = crate::worker::options(&s.machine, &worker)?;
                let root = opts.paths.data_dir.clone();
                {
                    let mut state = s.state.lock().await;
                    let owner = state.owners.get(&worker.machine).context("owner missing")?;
                    ensure!(
                        !owner.revoked && owner.epoch == epoch,
                        "revoked during worker admission"
                    );
                    let key = format!("{scope}/{name}");
                    if let Some(old) = state.workers.get(&key) {
                        ensure!(
                            old.workspace_key == worker.workspace_key,
                            "workspace key changed without a new generation"
                        );
                    }
                    state.workers.insert(key, worker.clone());
                    s.persist(&mut state).await?;
                }
                wire::write(
                    &mut send,
                    &Reply::Ok(
                        json!({"root":root,"features":FEATURES,"build":&worker.build.sha256[..16]}),
                    ),
                )
                .await?;
                // Revocation is also checked by the worker before opening its spool.
                crate::worker::relay(&s.machine, &worker, replace, tokio::io::join(recv, send))
                    .await?;
            }
            request => {
                let close = matches!(&request,RemoteRequest::Admin{certificate,admin:Admin::Revoke{machine}} if machine==&certificate.binding.machine);
                let value = s.remote_control(&c, request, Some(epoch)).await?;
                wire::write(&mut send, &Reply::Ok(value)).await?;
                send.finish()?;
                if close {
                    let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                    c.close(2u32.into(), b"hub revoked");
                    s.retire_if_unused().await;
                }
            }
        }
        Ok(())
    }
    async fn register_session(
        &self,
        certificate: &Certificate,
        connection: &iroh::endpoint::Connection,
        epoch: u64,
    ) -> Result<()> {
        ensure!(
            !self.failed.load(std::sync::atomic::Ordering::Acquire),
            "service state could not be saved"
        );
        certificate.verify(connection.remote_id())?;
        let state = self.state.lock().await;
        let owner = state
            .owners
            .get(&certificate.binding.machine)
            .context("hub is not paired")?;
        ensure!(
            owner.committed && !owner.revoked && owner.epoch == epoch,
            "hub trust is pending, stale or revoked"
        );
        let mut sessions = self.sessions.lock().await;
        let sessions = sessions
            .entry(certificate.binding.machine.clone())
            .or_default();
        sessions.retain(|_, c| c.close_reason().is_none());
        ensure!(
            sessions.len() < 16 || sessions.contains_key(&connection.stable_id()),
            "too many hub connections"
        );
        sessions.insert(connection.stable_id(), connection.clone());
        Ok(())
    }
    async fn authorize_at(
        &self,
        certificate: &Certificate,
        peer: iroh::EndpointId,
        epoch: u64,
    ) -> Result<(u64, String)> {
        let (current, scope) = self.authorize(certificate, peer).await?;
        ensure!(current == epoch, "trust grant was replaced or revoked");
        Ok((current, scope))
    }
    async fn authorize(
        &self,
        certificate: &Certificate,
        peer: iroh::EndpointId,
    ) -> Result<(u64, String)> {
        certificate.verify(peer)?;
        let state = self.state.lock().await;
        let owner = state
            .owners
            .get(&certificate.binding.machine)
            .context("hub is not paired")?;
        ensure!(
            owner.committed && !owner.revoked,
            "hub trust is pending or revoked"
        );
        self.active()?;
        ensure!(
            !self
                .machine
                .fence(&certificate.binding.machine, owner.epoch)
                .try_exists()?,
            "trust grant retired"
        );
        Ok((owner.epoch, certificate.scope()))
    }
    fn repo_path(
        &self,
        certificate: &Certificate,
        scope: &str,
        name: &str,
        project: &str,
    ) -> Result<PathBuf> {
        twr_core::runner::validate_box_name(name)?;
        ensure!(
            project.len() == 64
                && project
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid managed project id"
        );
        let worker = Worker {
            epoch: 0,
            machine: certificate.binding.machine.clone(),
            scope: scope.into(),
            workspace_key: certificate.binding.workspace_key.clone(),
            box_name: name.into(),
            build: crate::worker::Build {
                sha256: "0".repeat(64),
                size: 1,
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                protocol: twr_core::runner::PROTOCOL_VERSION,
            },
        };
        Ok(crate::worker::options(&self.machine, &worker)?
            .paths
            .data_dir
            .join("repos")
            .join(project))
    }
    async fn remote_request(
        &self,
        c: &iroh::endpoint::Connection,
        request: &RemoteRequest,
    ) -> Result<Value> {
        let (mut send, mut recv) = c.open_bi().await?;
        wire::write(&mut send, request).await?;
        let Reply::Ok(value) = wire::read_with_timeout(&mut recv, Duration::from_secs(120)).await?
        else {
            anyhow::bail!("runner refused operation")
        };
        Ok(value)
    }
    async fn push_repo(
        &self,
        name: &str,
        workspace: &str,
        project: &str,
        repository: &std::path::Path,
        base: Option<&str>,
    ) -> Result<Value> {
        let (w, c, _runner) = self.linked(name, workspace).await?;
        let project = crate::bundle::project_id(project);
        let repo = crate::bundle::Repository(std::fs::canonicalize(repository)?);
        let commit = repo.resolve(base.unwrap_or("HEAD")).await?;
        let status = self
            .remote_request(
                &c,
                &RemoteRequest::Have {
                    certificate: w.certificate.clone(),
                    name: name.into(),
                    project: project.clone(),
                },
            )
            .await?;
        let tips: Vec<String> = serde_json::from_value(status["tips"].clone())?;
        if tips.contains(&commit) {
            return Ok(json!({"repo":status["repo"],"commit":commit}));
        }
        let directory = tempfile::tempdir_in(&self.machine.dir)?;
        let path = directory.path().join("source.bundle");
        let bundle = repo.create_bundle(&commit, &path, &tips).await?;
        let (mut send, mut recv) = c.open_bi().await?;
        wire::write(
            &mut send,
            &RemoteRequest::Import {
                certificate: w.certificate.clone(),
                name: name.into(),
                project,
                bundle: bundle.clone(),
            },
        )
        .await?;
        ensure!(
            matches!(wire::read::<Reply>(&mut recv).await?, Reply::Ok(_)),
            "runner refused repository transfer"
        );
        let file = crate::worker::Build {
            sha256: bundle.sha256,
            size: bundle.size,
            os: String::new(),
            arch: String::new(),
            protocol: 0,
        };
        crate::worker::send(&mut send, &path, &file).await?;
        send.finish()?;
        let Reply::Ok(result) = wire::read(&mut recv).await? else {
            anyhow::bail!("runner did not import repository")
        };
        Ok(result)
    }
    async fn pull_repo(
        &self,
        name: &str,
        workspace: &str,
        project: &str,
        repository: &std::path::Path,
        branch: &str,
        result: &str,
    ) -> Result<Value> {
        ensure!(
            !branch.starts_with('-') && branch.len() <= 256,
            "invalid result branch"
        );
        ensure!(
            result.len() <= 128
                && result
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid result identity"
        );
        let (w, c, _runner) = self.linked(name, workspace).await?;
        let (mut send, mut recv) = c.open_bi().await?;
        wire::write(
            &mut send,
            &RemoteRequest::Export {
                certificate: w.certificate.clone(),
                name: name.into(),
                project: crate::bundle::project_id(project),
                branch: branch.into(),
            },
        )
        .await?;
        let Reply::Ok(value) = wire::read(&mut recv).await? else {
            anyhow::bail!("runner refused result export")
        };
        let bundle: crate::bundle::Bundle = serde_json::from_value(value["bundle"].clone())?;
        bundle.validate()?;
        let directory = tempfile::tempdir_in(&self.machine.dir)?;
        let path = directory.path().join("results.bundle");
        let file = crate::worker::Build {
            sha256: bundle.sha256.clone(),
            size: bundle.size,
            os: String::new(),
            arch: String::new(),
            protocol: 0,
        };
        let temp = crate::worker::receive(&mut recv, &path, &file).await?;
        let reference = format!(
            "refs/twr/results/{}/{result}/{}",
            crate::bundle::project_id(name),
            bundle.commit
        );
        crate::bundle::Repository(std::fs::canonicalize(repository)?)
            .receive(temp.path(), &bundle, &reference)
            .await?;
        Ok(json!({"ref":reference,"commit":bundle.commit,"includes":"committed changes only"}))
    }
    async fn linked_control(
        &self,
        name: &str,
        workspace: &str,
    ) -> Result<(Arc<Workspace>, iroh::endpoint::Connection, Runner)> {
        let w = self.workspace(workspace).await?;
        let key = format!("{name}/{}", w.endpoint.id());
        let runner = self
            .state
            .lock()
            .await
            .runners
            .get(name)
            .context("runner not paired")?
            .clone();
        let mut controls = self.controls.lock().await;
        controls.retain(|_, (_, _, c)| c.close_reason().is_none());
        if let Some((receipt, w, c)) = controls.get(&key) {
            if crate::pairing::same_secret(receipt, &runner.receipt) {
                return Ok((w.clone(), c.clone(), runner));
            }
        }
        if let Some((_, _, c)) = controls.remove(&key) {
            c.close(0u32.into(), b"control grant replaced");
        }
        drop(controls);
        let (w, c, runner) = self.linked(name, workspace).await?;
        let mut controls = self.controls.lock().await;
        if let Some((receipt, old_w, old_c)) = controls.get(&key) {
            if crate::pairing::same_secret(receipt, &runner.receipt)
                && old_c.close_reason().is_none()
            {
                c.close(0u32.into(), b"control already connected");
                return Ok((old_w.clone(), old_c.clone(), runner));
            }
        }
        controls.insert(key, (runner.receipt.clone(), w.clone(), c.clone()));
        Ok((w, c, runner))
    }
    async fn linked(
        &self,
        name: &str,
        workspace: &str,
    ) -> Result<(Arc<Workspace>, iroh::endpoint::Connection, Runner)> {
        self.active()?;
        let runner = self
            .state
            .lock()
            .await
            .runners
            .get(name)
            .context("runner not paired")?
            .clone();
        let w = self.workspace(workspace).await?;
        let mut addr =
            iroh::EndpointAddr::new(runner.endpoint.parse()?).with_relay_url(runner.relay.parse()?);
        ensure!(
            runner.addresses.len() <= 64,
            "too many saved runner addresses"
        );
        for address in &runner.addresses {
            addr = addr.with_ip_addr(*address);
        }
        let c = tokio::time::timeout(
            Duration::from_secs(30),
            w.endpoint.connect(addr, crate::ALPN),
        )
        .await??;
        self.remote_request(
            &c,
            &RemoteRequest::Authenticate {
                certificate: w.certificate.clone(),
                receipt: runner.receipt.clone(),
            },
        )
        .await?;
        let state = self.state.lock().await;
        ensure!(
            state
                .runners
                .get(name)
                .is_some_and(|r| r.receipt == runner.receipt),
            "runner removed or replaced during connection"
        );
        let mut outgoing = self.outgoing.lock().await;
        let connections = outgoing.entry(name.into()).or_default();
        connections.retain(|c| c.close_reason().is_none());
        ensure!(
            connections.len() < 64,
            "too many outgoing runner connections"
        );
        connections.push(c.clone());
        Ok((w, c, runner))
    }
    async fn retry_retirements(&self) {
        let Ok(_single) = self.retirement_lock.try_lock() else {
            return;
        };
        let pending = self.state.lock().await.pending_revocations.clone();
        for (id, runner) in pending {
            let result = async {
                let w = self.workspace("default").await?;
                let mut address = iroh::EndpointAddr::new(runner.endpoint.parse()?)
                    .with_relay_url(runner.relay.parse()?);
                for ip in &runner.addresses {
                    address = address.with_ip_addr(*ip);
                }
                let c = tokio::time::timeout(
                    Duration::from_secs(10),
                    w.endpoint.connect(address, crate::ALPN),
                )
                .await??;
                let _guard = ConnectionGuard(c.clone());
                self.remote_request(
                    &c,
                    &RemoteRequest::Retire {
                        certificate: w.certificate.clone(),
                        receipt: runner.receipt,
                    },
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if result.is_ok() {
                let mut state = self.state.lock().await;
                state.pending_revocations.remove(&id);
                let _ = self.persist(&mut state).await;
            }
        }
    }
    async fn recover(&self, name: &str, workspace: &str) -> Result<()> {
        let (w, c, runner) = self.linked(name, workspace).await?;
        self.commit(&c, &w.certificate, &runner.receipt).await?;
        let mut state = self.state.lock().await;
        let current = state
            .runners
            .get_mut(name)
            .context("runner removed during recovery")?;
        ensure!(
            current.receipt == runner.receipt,
            "runner replaced during recovery"
        );
        current.committed = true;
        self.persist(&mut state).await
    }
    async fn open(
        &self,
        name: &str,
        workspace: &str,
        executable: &std::path::Path,
        replace: bool,
    ) -> Result<(crate::worker::Stream, PathBuf, String)> {
        let (w, c, runner) = self.linked(name, workspace).await?;
        if !runner.committed {
            self.commit(&c, &w.certificate, &runner.receipt).await?;
            let mut state = self.state.lock().await;
            state
                .runners
                .get_mut(name)
                .context("runner removed")?
                .committed = true;
            self.persist(&mut state).await?;
        }
        let path = executable.to_path_buf();
        let build = crate::worker::Build::executable(&path).await?;
        let (mut send, mut recv) = c.open_bi().await?;
        wire::write(
            &mut send,
            &RemoteRequest::Probe {
                certificate: w.certificate.clone(),
            },
        )
        .await?;
        let Reply::Ok(probe) = wire::read(&mut recv).await? else {
            anyhow::bail!("runner refused capabilities")
        };
        for feature in FEATURES {
            ensure!(
                probe["features"]
                    .as_array()
                    .is_some_and(|f| f.iter().any(|v| v == feature)),
                "runner lacks a mandatory linked-worker feature"
            );
        }
        let mut uploaded = false;
        if probe["os"] == build.os && probe["arch"] == build.arch {
            // Upload over an independent QUIC stream, never the worker command-result spool.
            let (mut send, mut recv) = c.open_bi().await?;
            wire::write(
                &mut send,
                &RemoteRequest::Install {
                    certificate: w.certificate.clone(),
                    build: build.clone(),
                },
            )
            .await?;
            let Reply::Ok(response) = wire::read(&mut recv).await? else {
                anyhow::bail!("runner refused worker upload")
            };
            if response["ready"] == true {
                crate::worker::send(&mut send, &path, &build).await?;
                send.finish()?;
                let Reply::Ok(installed) = wire::read::<Reply>(&mut recv).await? else {
                    anyhow::bail!("worker installation failed")
                };
                uploaded = installed["usable"] == true;
            } else {
                uploaded = response["usable"] == true;
            }
        }
        let build = if uploaded {
            build
        } else {
            let (source, _directory, path) = crate::source::export(&path).await?;
            let (mut send, mut recv) = c.open_bi().await?;
            wire::write(
                &mut send,
                &RemoteRequest::BuildState {
                    certificate: w.certificate.clone(),
                    source: source.clone(),
                },
            )
            .await?;
            let Reply::Ok(status) = wire::read(&mut recv).await? else {
                anyhow::bail!("runner refused source build")
            };
            if let Some(build) = status.get("build") {
                serde_json::from_value(build.clone())?
            } else {
                if let Some(error) = status["error"].as_str() {
                    anyhow::bail!("{error}")
                }
                if status["needs_source"] == true {
                    let (mut send, mut recv) = c.open_bi().await?;
                    wire::write(
                        &mut send,
                        &RemoteRequest::BuildSource {
                            certificate: w.certificate.clone(),
                            source: source.clone(),
                        },
                    )
                    .await?;
                    ensure!(
                        matches!(wire::read::<Reply>(&mut recv).await?, Reply::Ok(_)),
                        "runner refused source transfer"
                    );
                    let file = crate::worker::Build {
                        sha256: source.bundle.sha256,
                        size: source.bundle.size,
                        os: String::new(),
                        arch: String::new(),
                        protocol: 0,
                    };
                    crate::worker::send(&mut send, &path, &file).await?;
                    send.finish()?;
                    ensure!(
                        matches!(wire::read::<Reply>(&mut recv).await?, Reply::Ok(_)),
                        "runner did not accept native build"
                    );
                }
                anyhow::bail!(
                    "runner is building this hub's recorded source; it will connect when ready"
                )
            }
        };
        let (mut send, mut recv) = c.open_bi().await?;
        wire::write(
            &mut send,
            &RemoteRequest::Open {
                certificate: w.certificate.clone(),
                name: name.into(),
                build,
                features: FEATURES.iter().map(|s| s.to_string()).collect(),
                replace,
            },
        )
        .await?;
        let Reply::Ok(value) = wire::read(&mut recv).await? else {
            anyhow::bail!("runner refused worker")
        };
        let root = PathBuf::from(
            value["root"]
                .as_str()
                .context("runner did not provide managed paths")?,
        );
        ensure!(root.is_absolute(), "runner returned a relative root");
        let actual = value["build"]
            .as_str()
            .context("worker build identity missing")?
            .to_string();
        Ok((crate::worker::Stream::new(c, send, recv), root, actual))
    }
    async fn remote_control(
        self: &Arc<Self>,
        connection: &iroh::endpoint::Connection,
        request: RemoteRequest,
        epoch: Option<u64>,
    ) -> Result<Value> {
        ensure!(
            !self.failed.load(std::sync::atomic::Ordering::Acquire),
            "service state could not be saved"
        );
        match request {
            RemoteRequest::Admin { certificate, admin } => {
                self.authorize_at(
                    &certificate,
                    connection.remote_id(),
                    epoch.context("link authentication missing")?,
                )
                .await?;
                match admin {
                    Admin::Status => {
                        let mut value = self.control(Request::Status).await?;
                        value["requesting_machine"] = certificate.binding.machine.into();
                        Ok(value)
                    }
                    Admin::Invite => self.control(Request::UseAsRunner).await,
                    Admin::Cancel { session } => {
                        self.control(Request::CancelPairing { session }).await
                    }
                    Admin::Revoke { machine } => {
                        // A self-revocation closes this control stream at the durable fence.
                        // Other owners stay connected and can confirm the resulting status.
                        self.revoke_machine(&machine, Some(connection.stable_id()))
                            .await
                    }
                }
            }
            RemoteRequest::Pressure {
                certificate,
                release,
            } => {
                self.authorize_at(
                    &certificate,
                    connection.remote_id(),
                    epoch.context("link authentication missing")?,
                )
                .await?;
                let resources = self.machine.dir.join("runner/leases");
                let available = tokio::task::spawn_blocking(move || -> Result<bool> {
                    Ok(twr_runner::managed::Managed::pressure(&resources)
                        && twr_runner::managed::Managed::at_capacity(&resources)?)
                })
                .await??;
                let now = std::time::Instant::now();
                let mut permit = self.retirement_until.lock().await;
                if let Some(release) = release {
                    if permit.1.as_ref() == Some(&release) {
                        *permit = (now, None);
                    }
                    return Ok(json!({"waiting":false}));
                }
                let claim = if permit.0 <= now && available {
                    let claim = hex::encode(crate::random::<16>()?);
                    *permit = (now + Duration::from_secs(10), Some(claim.clone()));
                    Some(claim)
                } else {
                    None
                };
                Ok(json!({"waiting":claim.is_some(),"claim":claim}))
            }

            RemoteRequest::CheckUpdate { certificate } => {
                self.authorize_at(
                    &certificate,
                    connection.remote_id(),
                    epoch.context("link authentication missing")?,
                )
                .await?;
                self.update_requested
                    .store(true, std::sync::atomic::Ordering::Release);
                Ok(json!({"checking":true}))
            }
            RemoteRequest::Authenticate { .. } | RemoteRequest::Retire { .. } => {
                anyhow::bail!("link is already authenticated")
            }
            RemoteRequest::Import { .. }
            | RemoteRequest::Export { .. }
            | RemoteRequest::BuildSource { .. }
            | RemoteRequest::Install { .. }
            | RemoteRequest::Open { .. } => unreachable!(),
            RemoteRequest::Have {
                certificate,
                name,
                project,
            } => {
                let (_, scope) = self
                    .authorize_at(
                        &certificate,
                        connection.remote_id(),
                        epoch.context("link authentication missing")?,
                    )
                    .await?;
                let root = self.repo_path(&certificate, &scope, &name, &project)?;
                let tip = crate::bundle::Repository(root.clone())
                    .resolve("refs/twr/source")
                    .await
                    .ok();
                Ok(json!({"repo":root,"tips":tip.into_iter().collect::<Vec<_>>()}))
            }
            RemoteRequest::Probe { certificate } => {
                self.authorize_at(
                    &certificate,
                    connection.remote_id(),
                    epoch.context("link authentication missing")?,
                )
                .await?;
                Ok(
                    json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"target":crate::source::host_target(),"features":FEATURES,"protocol":twr_core::runner::PROTOCOL_VERSION}),
                )
            }
            RemoteRequest::BuildState {
                certificate,
                source,
            } => {
                let _bulk = self.bulk.acquire().await?;
                source.validate()?;
                let (grant, scope) = self
                    .authorize_at(
                        &certificate,
                        connection.remote_id(),
                        epoch.context("link authentication missing")?,
                    )
                    .await?;
                if let Some(build) = crate::source::cached(&self.machine, &scope, &source).await? {
                    return Ok(json!({"build":build}));
                }
                let id = format!(
                    "{scope}/{grant}/{}",
                    source.id(crate::source::host_target())?
                );
                let jobs = self.builds.lock().await;
                match jobs.get(&id) {
                    Some(BuildJob::Running) => Ok(json!({"building":true})),
                    Some(BuildJob::Failed(message, at))
                        if at.elapsed() < Duration::from_secs(60) =>
                    {
                        Ok(json!({"error":message}))
                    }
                    _ => Ok(json!({"needs_source":true})),
                }
            }
            RemoteRequest::Pair {
                session,
                admission,
                transcript,
                proof,
                signature,
            } => {
                let mut pair = self.window.lock().await;
                let p = pair.as_mut().context("no pairing window")?;
                p.window.admit(&session, &admission)?;
                let exporter = network::exporter(connection)?;
                let verification = (|| -> Result<()> {
                    transcript.validate(
                        &p.derived,
                        connection.remote_id(),
                        iroh::SecretKey::from_bytes(&crate::state::secret(
                            &self.machine.dir.join("identity/runner-key"),
                        )?)
                        .public(),
                        &p.window.session,
                        p.window.expires,
                    )?;
                    transcript.verify(&p.derived, &exporter, "hub", &proof)?;
                    let mut signed = serde_json::to_vec(&transcript)?;
                    signed.extend(exporter);
                    ring::signature::UnparsedPublicKey::new(
                        &ring::signature::ED25519,
                        hex::decode(&transcript.certificate.binding.machine)?,
                    )
                    .verify(&signed, &hex::decode(signature)?)
                    .map_err(|_| anyhow::anyhow!("machine possession proof failed"))?;
                    Ok(())
                })();
                if let Err(e) = verification {
                    p.window.failed();
                    return Err(e);
                }
                let receipt = hex::encode(crate::random::<32>()?);
                let mut state = self.state.lock().await;
                ensure!(
                    state.owners.len() < 128
                        || state
                            .owners
                            .contains_key(&transcript.certificate.binding.machine),
                    "paired hub limit reached"
                );
                let key = transcript.certificate.binding.machine.clone();
                let old_epoch = state.owners.get(&key).map(|o| o.epoch);
                ensure!(transcript.replace || !state.owners.get(&key).is_some_and(|o|o.committed&&!o.revoked),"machine already paired; recover the saved link or explicitly replace its access");
                p.window.consume()?;
                if let Some(old) = state.owners.get(&key) {
                    self.machine.retire(&key, old.epoch)?;
                }
                if let Some(sessions) = self.sessions.lock().await.remove(&key) {
                    for c in sessions.into_values() {
                        c.close(2u32.into(), b"trust grant replaced");
                    }
                }
                let epoch = match state.owners.get(&key) {
                    Some(owner) => owner
                        .epoch
                        .checked_add(1)
                        .context("trust epoch exhausted")?,
                    None => 0,
                };
                state.owners.insert(
                    key.clone(),
                    Owner {
                        receipt: receipt.clone(),
                        committed: false,
                        revoked: false,
                        epoch,
                    },
                );
                self.persist(&mut state).await?;
                if let Some(old_epoch) = old_epoch {
                    self.retire_processes(&key, old_epoch);
                }
                let confirmation = transcript.proof(&p.derived, &exporter, "runner")?;
                *pair = None;
                Ok(
                    json!({"proof":confirmation,"receipt":receipt,"epoch":epoch,"addresses":self.runner.lock().await.as_ref().map(|e|e.addr().ip_addrs().copied().take(64).collect::<Vec<_>>()).unwrap_or_default()}),
                )
            }
            RemoteRequest::Commit {
                certificate,
                receipt,
            } => {
                certificate.verify(connection.remote_id())?;
                let mut state = self.state.lock().await;
                let owner = state
                    .owners
                    .get_mut(&certificate.binding.machine)
                    .context("hub not paired")?;
                ensure!(
                    epoch.is_none_or(|epoch| epoch == owner.epoch),
                    "trust epoch changed"
                );
                ensure!(
                    !owner.revoked && crate::pairing::same_secret(&owner.receipt, &receipt),
                    "pairing receipt was revoked or replaced"
                );
                owner.committed = true;
                let epoch = owner.epoch;
                self.persist(&mut state).await?;
                Ok(json!({"committed":true,"epoch":epoch}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn endpoint() -> Result<Endpoint> {
        Ok(Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::LOCALHOST,
                0,
            ))?
            .alpns(vec![crate::ALPN.to_vec()])
            .bind()
            .await?)
    }
    async fn fixture(dir: &std::path::Path) -> Result<Arc<Service>> {
        fixture_with_publisher(dir, Arc::new(|_| Box::pin(async { Ok(()) }))).await
    }
    async fn fixture_with_publisher(
        dir: &std::path::Path,
        publisher: Publisher,
    ) -> Result<Arc<Service>> {
        let paths = Paths::with_home(dir);
        let machine = Machine::of(&paths);
        machine.init()?;
        ensure_private_dir(&machine.dir.join("identity"))?;
        let identity = HubKey::load_or_create(&machine.dir.join("identity/link-key"))?;
        let service = Arc::new(Service {
            publisher,
            builds: Default::default(),
            peer_guard: Arc::new(|_| true),
            unauth: Arc::new(Semaphore::new(16)),
            kdf: Semaphore::new(1),
            bulk: Semaphore::new(1),
            update_lock: Mutex::new(()),
            retirement_lock: Mutex::new(()),
            retirement_until: Mutex::new((std::time::Instant::now(), None)),
            controls: Mutex::new(HashMap::new()),
            pairing_control: Mutex::new(()),
            tasks: Default::default(),
            shutting: Default::default(),
            update_requested: Default::default(),
            upgrade: Mutex::new(None),
            failed: Default::default(),
            sessions: Default::default(),
            outgoing: Default::default(),
            state: Mutex::new(machine.load()?),
            machine,
            paths,
            identity,
            runner: Mutex::new(None),
            workspaces: Mutex::new(HashMap::new()),
            window: Mutex::new(None),
            stopping: Default::default(),
        });
        let secret = crate::random::<32>()?;
        crate::state::atomic_write(&service.machine.dir.join("identity/runner-key"), &secret)?;
        let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(iroh::SecretKey::from_bytes(&secret))
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::LOCALHOST,
                0,
            ))?
            .alpns(vec![crate::ALPN.to_vec()])
            .bind()
            .await?;
        *service.runner.lock().await = Some(endpoint.clone());
        let weak = Arc::downgrade(&service);
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                if let (Some(service), Ok(c)) = (weak.upgrade(), incoming.await) {
                    tokio::spawn(async move {
                        let _ = service.remote(c).await;
                    });
                }
            }
        });
        Ok(service)
    }
    async fn connect_fixture(
        service: &Arc<Service>,
        hub: &Endpoint,
    ) -> Result<iroh::endpoint::Connection> {
        let endpoint = service.runner.lock().await.as_ref().unwrap().clone();
        let address = endpoint.addr();
        Ok(hub.connect(address, crate::ALPN).await?)
    }
    async fn request(
        connection: &iroh::endpoint::Connection,
        request: &RemoteRequest,
    ) -> Result<Value> {
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::write(&mut send, request).await?;
        match wire::read::<Reply>(&mut recv).await? {
            Reply::Ok(value) => Ok(value),
            Reply::Error(error) => anyhow::bail!("{error}"),
        }
    }
    async fn pairing_attempt(
        service: &Arc<Service>,
        hub: &Endpoint,
        machine: &HubKey,
        certificate: &Certificate,
        replace: bool,
    ) -> Result<(iroh::endpoint::Connection, Result<Value>)> {
        let c = connect_fixture(service, hub).await?;
        let code = Code::parse("4HK7-M2QP-9XTR")?;
        let derived = code.derive()?;
        let window = Window::new()?;
        let t = Transcript {
            version: 1,
            replace,
            hub: hub.id().to_string(),
            runner: c.remote_id().to_string(),
            locator: derived.locator_key().public_key().to_string(),
            session: window.session.clone(),
            expires: window.expires,
            certificate: certificate.clone(),
        };
        let admission = window.admission.clone();
        *service.window.lock().await = Some(PairWindow {
            window,
            derived: code.derive()?,
        });
        let exporter = network::exporter(&c)?;
        let mut signed = serde_json::to_vec(&t)?;
        signed.extend(exporter);
        let reply = request(
            &c,
            &RemoteRequest::Pair {
                session: t.session.clone(),
                admission,
                transcript: t.clone(),
                proof: t.proof(&derived, &exporter, "hub")?,
                signature: hex::encode(machine.sign(&signed)),
            },
        )
        .await;
        if let Ok(value) = &reply {
            t.verify(
                &derived,
                &exporter,
                "runner",
                value["proof"].as_str().context("runner proof missing")?,
            )?;
        }
        Ok((c, reply))
    }
    #[tokio::test]
    async fn publication_failure_retires_the_private_window_and_endpoint() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = attempts.clone();
        let service = fixture_with_publisher(
            temp.path(),
            Arc::new(move |_| {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { anyhow::bail!("private publisher unavailable") })
            }),
        )
        .await?;
        let window = Window::new()?;
        let record = crate::locator::Record {
            version: 1,
            endpoint: service
                .runner
                .lock()
                .await
                .as_ref()
                .unwrap()
                .id()
                .to_string(),
            relay: "http://127.0.0.1:9".into(),
            expires_unix: window.expires,
            session: window.session.clone(),
            admission: window.admission.clone(),
        };
        assert!(service
            .publish_window(
                "private-test-code".into(),
                Code::parse("4HK7-M2QP-9XTR")?.derive()?,
                window,
                record
            )
            .await
            .is_err());
        assert!(service.window.lock().await.is_none());
        assert!(service.runner.lock().await.is_none());
        assert!(attempts.load(std::sync::atomic::Ordering::Relaxed) > 0);
        Ok(())
    }
    #[tokio::test]
    async fn retirement_obeys_the_pairing_lock_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let service = fixture(temp.path()).await?;
        let window = service.window.lock().await;
        let started = Arc::new(tokio::sync::Notify::new());
        let s = service.clone();
        let notify = started.clone();
        let task = tokio::spawn(async move {
            notify.notify_one();
            s.retire_if_unused().await;
        });
        started.notified().await;
        tokio::task::yield_now().await;
        let state = tokio::time::timeout(Duration::from_secs(1), service.state.lock())
            .await
            .context("retirement held state while waiting for a pairing window")?;
        drop(state);
        drop(window);
        tokio::time::timeout(Duration::from_secs(2), task).await??;
        Ok(())
    }
    #[tokio::test]
    async fn real_quic_pairing_durable_receipts_recovery_single_use_and_revocation() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(90), async {
            let dir = tempfile::tempdir()?;
            let service = fixture(dir.path()).await?;
            let hub = endpoint().await?;
            let connection = connect_fixture(&service, &hub).await?;
            let machine = HubKey::generate()?;
            let workspace_file = dir.path().join("test-box-key");
            let workspace = HubKey::load_or_create(&workspace_file)?;
            let certificate = Certificate::issue(
                &machine,
                &workspace,
                hub.id(),
                "01".repeat(16),
                "02".repeat(16),
            )?;
            let code = Code::parse("4HK7-M2QP-9XTR")?;
            let derived = code.derive()?;
            let window = Window::new()?;
            let t = Transcript {
                version: 1,
                replace: false,
                hub: hub.id().to_string(),
                runner: service
                    .runner
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .id()
                    .to_string(),
                locator: derived.locator_key().public_key().to_string(),
                session: window.session.clone(),
                expires: window.expires,
                certificate: certificate.clone(),
            };
            let admission = window.admission.clone();
            *service.window.lock().await = Some(PairWindow {
                window,
                derived: code.derive()?,
            });
            let exporter = network::exporter(&connection)?;
            let mut signed = serde_json::to_vec(&t)?;
            signed.extend(exporter);
            let pair = RemoteRequest::Pair {
                session: t.session.clone(),
                admission,
                transcript: t.clone(),
                proof: t.proof(&derived, &exporter, "hub")?,
                signature: hex::encode(machine.sign(&signed)),
            };
            let reply = request(&connection, &pair).await?;
            t.verify(
                &derived,
                &exporter,
                "runner",
                reply["proof"].as_str().unwrap(),
            )?;
            assert!(!service.machine.load()?.owners[&machine.public_hex()].committed);
            let receipt = reply["receipt"].as_str().unwrap().to_string();
            // The first connection can disappear after prepare. A new authenticated QUIC
            // connection commits the exact durable receipt without needing the code again.
            connection.close(0u32.into(), b"interrupted after prepare");
            let reconnect = connect_fixture(&service, &hub).await?;
            request(
                &reconnect,
                &RemoteRequest::Commit {
                    certificate: certificate.clone(),
                    receipt: receipt.clone(),
                },
            )
            .await?;
            assert!(service.machine.load()?.owners[&machine.public_hex()].committed);
            request(
                &reconnect,
                &RemoteRequest::Commit {
                    certificate: certificate.clone(),
                    receipt: receipt.clone(),
                },
            )
            .await?;
            assert!(service.authorize(&certificate, hub.id()).await.is_ok());
            assert!(service.window.lock().await.is_none());
            let hub_dir = tempfile::tempdir()?;
            let hub_service = fixture(hub_dir.path()).await?;
            hub_service.workspaces.lock().await.insert(
                "default".into(),
                Arc::new(Workspace {
                    endpoint: hub.clone(),
                    certificate: certificate.clone(),
                }),
            );
            let workspace_paths = hub_service.paths.workspace("default")?;
            ensure_private_dir(&workspace_paths.data_dir)?;
            let mut workspace_identity = vec![1; 16];
            workspace_identity.extend([2; 16]);
            workspace_identity.extend(hub.secret_key().to_bytes());
            crate::state::atomic_write(
                &workspace_paths.data_dir.join("link-identity"),
                &workspace_identity,
            )?;
            crate::state::atomic_write(
                &workspace_paths.data_dir.join("box-key"),
                &std::fs::read(&workspace_file)?,
            )?;
            let runner_endpoint = service.runner.lock().await.as_ref().unwrap().clone();
            hub_service.state.lock().await.runners.insert(
                "desk".into(),
                Runner {
                    endpoint: runner_endpoint.id().to_string(),
                    relay: "http://127.0.0.1:9".into(),
                    receipt: receipt.clone(),
                    committed: true,
                    addresses: runner_endpoint.addr().ip_addrs().copied().collect(),
                },
            );
            // Concurrent control operations retain one authenticated connection.
            let (_, cached, _) = hub_service.linked_control("desk", "default").await?;
            let (management, pressure) = tokio::join!(
                hub_service.control(Request::Manage {
                    name: "desk".into(),
                    workspace: "default".into(),
                    admin: Admin::Status
                }),
                hub_service.control(Request::Pressure {
                    name: "desk".into(),
                    workspace: "default".into(),
                    release: None
                })
            );
            management?;
            pressure?;
            let (_, reused, _) = hub_service.linked_control("desk", "default").await?;
            assert_eq!(cached.stable_id(), reused.stable_id());
            assert!(cached.close_reason().is_none());
            let epoch = service.state.lock().await.owners[&machine.public_hex()].epoch;
            let record = crate::locator::Record {
                version: 1,
                endpoint: runner_endpoint.id().to_string(),
                relay: "http://127.0.0.1:9".into(),
                expires_unix: crate::now() + 900,
                session: "01".repeat(16),
                admission: "02".repeat(16),
            };
            hub_service
                .connect_record(
                    Code::parse("4HK7-M2QP-9XTR")?.derive()?,
                    record,
                    "second-name".into(),
                    "default".into(),
                    false,
                )
                .await?;
            assert_eq!(
                service.state.lock().await.owners[&machine.public_hex()].epoch,
                epoch,
                "adding an alias rotated machine trust"
            );
            hub_service
                .control(Request::Forget {
                    name: "second-name".into(),
                })
                .await?;
            assert!(
                hub_service
                    .state
                    .lock()
                    .await
                    .pending_revocations
                    .is_empty(),
                "forgetting one alias revoked shared trust"
            );
            let (_attempt, reply) =
                pairing_attempt(&service, &hub, &machine, &certificate, false).await?;
            assert!(reply.is_err(), "an active grant was silently replaced");
            assert!(!service.machine.fence(&machine.public_hex(), epoch).exists());
            assert!(service.window.lock().await.is_some());
            service.cancel_pairing(None).await?;
            let repo_path = hub_dir.path().join("project");
            std::fs::create_dir(&repo_path)?;
            let local = crate::bundle::Repository(repo_path.clone());
            local.git(&["init", "."]).await?;
            std::fs::write(repo_path.join("source"), "original")?;
            local.git(&["add", "."]).await?;
            local.git(&["commit", "-m", "source"]).await?;
            let original = local.resolve("HEAD").await?;
            let index = std::fs::read(repo_path.join(".git/index"))?;
            let synced = hub_service
                .push_repo("desk", "default", "project", &repo_path, None)
                .await?;
            assert_eq!(synced["commit"], original);
            let remote_repo =
                crate::bundle::Repository(PathBuf::from(synced["repo"].as_str().unwrap()));
            let worktree = remote_repo
                .0
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("worktrees/test");
            let mut cmd = crate::bundle::command(&remote_repo.0);
            cmd.args(["worktree", "add", "-b", "result"])
                .arg(&worktree)
                .arg(&original);
            crate::bundle::run(cmd).await?;
            let work = crate::bundle::Repository(worktree.clone());
            std::fs::write(worktree.join("result"), "committed result")?;
            work.git(&["add", "."]).await?;
            work.git(&["commit", "-m", "result"]).await?;
            let result_commit = work.resolve("HEAD").await?;
            let pulled = hub_service
                .pull_repo(
                    "desk",
                    "default",
                    "project",
                    &repo_path,
                    "result",
                    "thread-test",
                )
                .await?;
            assert_eq!(pulled["commit"], result_commit);
            assert_eq!(local.resolve("HEAD").await?, original);
            assert_eq!(std::fs::read(repo_path.join(".git/index"))?, index);
            assert!(!repo_path.join("result").exists());
            hub_service
                .runner
                .lock()
                .await
                .as_ref()
                .unwrap()
                .close()
                .await;
            // Retirement permits are global, and canceled generation tickets disappear.
            let resources = service.machine.dir.join("runner/leases");
            crate::state::atomic_write(&resources.join("slots.json"), b"1")?;
            let managed = twr_runner::managed::Managed {
                root: remote_repo
                    .0
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf(),
                resources: resources.clone(),
                revocation: service.machine.fence(&machine.public_hex(), epoch),
            };
            let live = managed.admit("resident", "resident-run")?;
            assert!(managed.admit("waiting", "waiting-run").is_err());
            let claim = request(
                &reconnect,
                &RemoteRequest::Pressure {
                    certificate: certificate.clone(),
                    release: None,
                },
            )
            .await?;
            assert_eq!(claim["waiting"], true);
            assert_eq!(
                request(
                    &reconnect,
                    &RemoteRequest::Pressure {
                        certificate: certificate.clone(),
                        release: None
                    }
                )
                .await?["waiting"],
                false
            );
            // A busy candidate releases the permit; stale releases cannot cancel a newer claim.
            request(
                &reconnect,
                &RemoteRequest::Pressure {
                    certificate: certificate.clone(),
                    release: Some(claim["claim"].as_str().unwrap().into()),
                },
            )
            .await?;
            let next = request(
                &reconnect,
                &RemoteRequest::Pressure {
                    certificate: certificate.clone(),
                    release: None,
                },
            )
            .await?;
            assert_eq!(next["waiting"], true);
            request(
                &reconnect,
                &RemoteRequest::Pressure {
                    certificate: certificate.clone(),
                    release: Some(claim["claim"].as_str().unwrap().into()),
                },
            )
            .await?;
            assert_eq!(
                request(
                    &reconnect,
                    &RemoteRequest::Pressure {
                        certificate: certificate.clone(),
                        release: None
                    }
                )
                .await?["waiting"],
                false
            );
            drop(live);
            managed.cancel_admission("waiting", Some("waiting-run"))?;
            assert!(!twr_runner::managed::Managed::pressure(&resources));
            let (replacement, reply) =
                pairing_attempt(&service, &hub, &machine, &certificate, true).await?;
            let new_receipt = reply?["receipt"]
                .as_str()
                .context("new receipt missing")?
                .to_string();
            assert!(service.machine.fence(&machine.public_hex(), epoch).exists());
            service
                .commit(&replacement, &certificate, &new_receipt)
                .await?;
            assert!(request(
                &reconnect,
                &RemoteRequest::Probe {
                    certificate: certificate.clone()
                }
            )
            .await
            .is_err());
            let stale = connect_fixture(&service, &hub).await?;
            assert!(request(
                &stale,
                &RemoteRequest::Authenticate {
                    certificate: certificate.clone(),
                    receipt: receipt.clone()
                }
            )
            .await
            .is_err());
            let fresh = connect_fixture(&service, &hub).await?;
            request(
                &fresh,
                &RemoteRequest::Authenticate {
                    certificate: certificate.clone(),
                    receipt: new_receipt,
                },
            )
            .await?;
            request(
                &fresh,
                &RemoteRequest::Probe {
                    certificate: certificate.clone(),
                },
            )
            .await?;
            request(
                &fresh,
                &RemoteRequest::Admin {
                    certificate: certificate.clone(),
                    admin: Admin::Revoke {
                        machine: machine.public_hex(),
                    },
                },
            )
            .await?;
            tokio::time::timeout(Duration::from_secs(6), async {
                while service.runner.lock().await.is_some() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            assert!(service.machine.load()?.owners[&machine.public_hex()].revoked);
            assert!(service.authorize(&certificate, hub.id()).await.is_err());
            let forged = Certificate::issue(
                &HubKey::generate()?,
                &workspace,
                hub.id(),
                "01".repeat(16),
                "02".repeat(16),
            )?;
            assert!(service.authorize(&forged, hub.id()).await.is_err());
            hub.close().await;
            if let Some(endpoint) = service.runner.lock().await.as_ref() {
                endpoint.close().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
}

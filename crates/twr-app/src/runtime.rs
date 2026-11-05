//! Desktop lifecycle operations; the UI never owns the hub process. Probe by typed I/O
//! errors, never by display strings, and leave a busy or differently owned socket alone.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use twr_core::{client::Client, paths::Paths};

/// Refuses a hub connection whose `hello` answer did not make it the user's (`identity` is
/// `user:...`). The hub stays authoritative: an agent-launched process is never disguised as a
/// user. The error says why the hub counts it as an agent when the hub says so (`why`, the
/// process and what of its ancestry made it one), else the identity it gave, so a refusal can
/// be understood from the app's own message.
pub fn ensure_user(hello: &Value) -> Result<()> {
    let identity = hello["identity"].as_str().unwrap_or_default();
    if identity.starts_with("user:") {
        return Ok(());
    }
    let why = match (hello["why"].as_str(), identity) {
        (Some(why), _) => format!(": it is {why}"),
        (None, "") => String::new(),
        (None, identity) => format!(" (it says {identity})"),
    };
    anyhow::bail!(
        "The hub did not recognize this desktop process as your user{why}. Open twr from your \
         desktop launcher."
    )
}

enum Probe {
    Ready(Value),
    Absent,
    Busy,
}
async fn probe_inner(socket: &Path, connected: &mut bool) -> Result<Probe> {
    twr_core::trace::mark("desktop: hub probe connecting");
    let mut c = match Client::connect(socket).await {
        Ok(c) => c,
        Err(e)
            if e.chain().any(|e| {
                e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                })
            }) =>
        {
            return Ok(Probe::Absent)
        }
        Err(e)
            if e.chain().any(|e| {
                e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
            }) =>
        {
            return Ok(Probe::Busy)
        }
        Err(e) => return Err(e),
    };
    *connected = true;
    twr_core::trace::mark("desktop: hub probe connected");
    // Keep one connection while the hub performs its peer-identity check. Starting
    // fresh probes during that work compounds process-table load on macOS.
    let result = async {
        let hello = c.hello(json!({"client":"desktop"})).await?;
        twr_core::trace::mark("desktop: hub probe authenticated");
        ensure_user(&hello)?;
        c.call("hub.info", json!({})).await
    }
    .await;
    match result {
        Ok(info) => Ok(Probe::Ready(info)),
        Err(e)
            if e.downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|r| r.code == twr_core::proto::codes::STOPPING) =>
        {
            Ok(Probe::Busy)
        }
        Err(e) => Err(e),
    }
}

async fn probe(socket: &Path, deadline: tokio::time::Instant) -> Result<Probe> {
    let mut connected = false;
    match tokio::time::timeout_at(deadline, probe_inner(socket, &mut connected)).await {
        Ok(result) => result,
        Err(_) if connected => anyhow::bail!("The hub accepts connections but did not answer the startup handshake. Check hub.log in the workspace data directory."),
        Err(_) => anyhow::bail!("The hub socket did not respond before the startup deadline. Check hub.log in the workspace data directory."),
    }
}

/// On macOS a process inherits its parent's exception ports, and the desktop app's are those of
/// its crash reporter (Crashpad): the binaries the app runs call this first, so what they start
/// uses the system's crash reporting (`twr_adapters::process::forget_inherited_crash_handler`,
/// which every `twr` calls too). Elsewhere it does nothing.
pub use twr_link::forget_inherited_crash_handler;

fn spawn_hub(paths: &Paths, socket: &Path, binary: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    anyhow::ensure!(
        binary.is_absolute() && binary.file_name().is_some_and(|s| s == "twr"),
        "A stable absolute twr executable is required"
    );
    // A workspace that was removed (the root slot's too) is not made again here: only `twr ws
    // new` makes one, and the hub checks again under its lock.
    anyhow::ensure!(
        paths.exists(),
        "Workspace does not exist; create it before connecting"
    );
    match paths.workspace {
        None => twr_core::paths::ensure_private_dir(&paths.data_dir)?,
        Some(_) => twr_core::paths::check_private_dir(&paths.data_dir)?,
    }
    let log = twr_core::paths::open_private_append(&paths.hub_log())?;
    let mut command = std::process::Command::new(binary);
    command
        .args(["--workspace", paths.workspace_id(), "--socket"])
        .arg(socket)
        .arg("hub")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env_remove("TWR_AGENT")
        .env_remove("TWR_DEVICE_TOKEN")
        .env_remove("TWR_MOBILE_TRANSPORT")
        .env_remove(twr_core::client::TEST_USER_TOKEN_ENV);
    twr_core::fork::prepare_fork();
    // SAFETY: setsid is async-signal-safe and touches no Rust allocation or locks, and so is
    // close_inherited_on_exec. The hub outlives the desktop app that runs this: it keeps none
    // of the app's descriptors.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        command.pre_exec(twr_core::fork::close_inherited_on_exec);
    }
    let mut child = command.spawn().context("Starting the hub")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub async fn ensure_hub(paths: &Paths, socket: &Path, binary: &Path) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut started = false;
    loop {
        match probe(socket, deadline).await? {
            Probe::Ready(info) => return Ok(info),
            Probe::Absent if !started => {
                spawn_hub(paths, socket, binary)?;
                started = true;
            }
            _ => {}
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "Hub did not start within 60 seconds. Check hub.log in the workspace data directory."
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::BufReader, net::UnixListener};
    use twr_core::proto::{read_frame, write_frame, Request, Response};

    /// A `hello` that is not the user's is refused with the hub's reason, when it gives one,
    /// else with the identity it gave; the user's passes.
    #[test]
    fn a_hello_that_is_not_the_users_is_refused_with_the_hubs_reason() {
        assert!(ensure_user(&json!({"identity": "user:desktop"})).is_ok());
        let why = "a process twr cannot trace (pid 7: the environment of pid 3, of its \
                   ancestry, which would show TWR_AGENT, cannot be read), which counts as an agent";
        let refused = |hello: Value| ensure_user(&hello).unwrap_err().to_string();
        assert_eq!(
            refused(json!({"identity": "agent:unknown", "why": why})),
            format!(
                "The hub did not recognize this desktop process as your user: it is {why}. \
                 Open twr from your desktop launcher."
            )
        );
        assert_eq!(
            refused(json!({"identity": "agent:w1"})),
            "The hub did not recognize this desktop process as your user (it says agent:w1). \
             Open twr from your desktop launcher."
        );
        assert_eq!(
            refused(json!({})),
            "The hub did not recognize this desktop process as your user. Open twr from your \
             desktop launcher."
        );
    }

    #[tokio::test]
    async fn running_hub_with_a_slow_greeting_is_not_mistaken_for_startup() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let hello: Request = read_frame(&mut read).await.unwrap().unwrap();
            assert_eq!(hello.method, "hello");
            tokio::time::sleep(Duration::from_secs(11)).await;
            write_frame(
                &mut write,
                &Response::ok(hello.id, json!({"identity":"user:test"})),
            )
            .await
            .unwrap();
            let info: Request = read_frame(&mut read).await.unwrap().unwrap();
            assert_eq!(info.method, "hub.info");
            write_frame(&mut write, &Response::ok(info.id, json!({"pid":123})))
                .await
                .unwrap();
        });
        let result = probe(
            &socket,
            tokio::time::Instant::now() + Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(matches!(result, Probe::Ready(info) if info["pid"] == 123));
        server.await.unwrap();
    }
}

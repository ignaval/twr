//! Desktop lifecycle operations; the UI never owns the hub process. Probe by typed I/O
//! errors, never by display strings, and leave a busy or differently owned socket alone.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use twr_core::{client::Client, paths::Paths};

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
        anyhow::ensure!(
            hello["identity"]
                .as_str()
                .is_some_and(|s| s.starts_with("user:")),
            "Open twr from your desktop launcher; this process is not recognized as your user"
        );
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
/// its crash reporter (Crashpad). A hub started from here would hold them for as long as it
/// runs: the app's crash handler, and the app's stdout and stderr it keeps, would outlive the
/// app, and the hub's crashes would be filed as the app's. The binaries the app runs call this
/// first, so what they start uses the system's crash reporting. Elsewhere it does nothing.
pub fn forget_inherited_crash_handler() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            // This task's port: what <mach/mach_init.h>'s mach_task_self() reads.
            static mach_task_self_: u32;
            // <mach/task.h>; the types are those of <mach/exception_types.h>.
            fn task_set_exception_ports(
                task: u32,
                exception_mask: u32,
                new_port: u32,
                behavior: i32,
                new_flavor: i32,
            ) -> i32;
        }
        // EXC_MASK_ALL | EXC_MASK_CRASH | EXC_MASK_CORPSE_NOTIFY, then without the last for a
        // system that refuses it.
        const MASKS: [u32; 2] = [0x3ffe, 0x1ffe];
        const EXCEPTION_DEFAULT: i32 = 1;
        #[cfg(target_arch = "aarch64")]
        const THREAD_STATE_NONE: i32 = 5;
        #[cfg(not(target_arch = "aarch64"))]
        const THREAD_STATE_NONE: i32 = 13;
        for mask in MASKS {
            // SAFETY: a Mach call on this task's own exception ports; MACH_PORT_NULL (0) puts
            // back the default handler. No memory is passed.
            let result = unsafe {
                task_set_exception_ports(
                    mach_task_self_,
                    mask,
                    0,
                    EXCEPTION_DEFAULT,
                    THREAD_STATE_NONE,
                )
            };
            if result == 0 {
                break;
            }
        }
    }
}

fn spawn_hub(paths: &Paths, socket: &Path, binary: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    anyhow::ensure!(
        binary.is_absolute() && binary.file_name().is_some_and(|s| s == "twr"),
        "A stable absolute twr executable is required"
    );
    if paths.workspace.is_some() {
        anyhow::ensure!(
            paths.data_dir.is_dir(),
            "Workspace does not exist; create it before connecting"
        );
    }
    twr_core::paths::ensure_private_dir(&paths.data_dir)?;
    let log = twr_core::paths::open_private_append(&paths.hub_log())?;
    let mut command = std::process::Command::new(binary);
    command
        .args(["--workspace", paths.workspace_name(), "--socket"])
        .arg(socket)
        .arg("hub")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env_remove("TWR_AGENT")
        .env_remove("TWR_DEVICE_TOKEN")
        .env_remove("TWR_MOBILE_TRANSPORT")
        .env_remove(twr_core::client::TEST_USER_TOKEN_ENV);
    // SAFETY: setsid is async-signal-safe and touches no Rust allocation or locks.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
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

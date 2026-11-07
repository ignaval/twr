//! Real managed-worker process, isolated data, private Unix sockets, no discovery or agent probes.
use anyhow::{Context, Result};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use twr_core::{paths::Paths, sealed::HubKey};
use twr_link::{
    service::{Reply, Request},
    state::Machine,
};

struct Reaped(Child);
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn managed_worker_reconnects_and_a_retired_grant_cannot_restart() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let machine = Machine {
        dir: temp.path().join("machine"),
        socket: temp.path().join("run/service.sock"),
    };
    machine.init()?;
    let key = HubKey::generate()?;
    let binary = Path::new(env!("CARGO_BIN_EXE_twr"));
    let build = twr_link::worker::Build::executable(binary).await?;
    let worker = twr_link::state::Worker {
        epoch: 3,
        machine: "ab".repeat(32),
        scope: format!("{}/{}", "ab".repeat(32), "cd".repeat(16)),
        workspace_key: key.public_hex(),
        box_name: "desk".into(),
        build,
    };
    let opts = twr_link::worker::options(&machine, &worker)?;
    let managed = opts.managed.as_ref().unwrap();
    let spawn = || -> Result<Reaped> {
        Ok(Reaped(
            Command::new(binary)
                .args([
                    "runner",
                    "--daemon",
                    "--box=desk",
                    "--hub-key",
                    &key.public_hex(),
                    "--daemon-socket",
                ])
                .arg(&opts.paths.socket)
                .arg("--data-dir")
                .arg(&opts.paths.data_dir)
                .arg("--lock")
                .arg(&opts.paths.lock)
                .arg("--log")
                .arg(&opts.paths.log)
                .arg("--managed-root")
                .arg(&managed.root)
                .arg("--resource-root")
                .arg(&managed.resources)
                .arg("--revocation-file")
                .arg(&managed.revocation)
                .env("HOME", temp.path())
                .env("TWR_HOME", temp.path().join("home"))
                .env("TWR_AGENT_PROBE", "off")
                .env("TWR_BOX_PROBE", "off")
                .env_remove("TWR_WORKSPACE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        ))
    };
    let mut child = spawn()?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if twr_runner::daemon::request(&opts.paths.socket, twr_runner::daemon::Request::Status)
                .await
                .is_ok()
            {
                break;
            }
            anyhow::ensure!(
                child.0.try_wait()?.is_none(),
                "worker exited while starting"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    // Dropping a hub transport leaves the same independently running worker available.
    for _ in 0..2 {
        let (io, status) =
            twr_runner::daemon::request(&opts.paths.socket, twr_runner::daemon::Request::Connect)
                .await?;
        assert_eq!(status.protocol, twr_core::runner::PROTOCOL_VERSION);
        let (sealed, hello) = tokio::time::timeout(
            Duration::from_secs(10),
            twr_core::sealed::hub_side(io, &key, "desk"),
        )
        .await??;
        assert_eq!(hello.box_name, "desk");
        drop(sealed);
        assert!(child.0.try_wait()?.is_none());
    }
    machine.retire(&worker.machine, worker.epoch)?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if child.0.try_wait()?.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(opts.paths.data_dir.exists(), "revocation retains work");
    let mut denied = spawn()?;
    let exit = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = denied.0.try_wait()? {
                return Ok::<_, anyhow::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert!(!exit.success(), "a permanently retired grant restarted");
    Ok(())
}

#[tokio::test]
async fn new_machine_service_is_dormant_and_disabled_preference_survives_restart() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths::with_home(temp.path());
    std::fs::create_dir_all(&paths.data_dir)?;
    let machine = Machine::of(&paths);
    machine.init()?;
    let task = tokio::spawn(twr_link::service::run(
        paths.data_dir.clone(),
        machine.socket.clone(),
        std::sync::Arc::new(|_| true),
    ));
    let mut io = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(io) = tokio::net::UnixStream::connect(&machine.socket).await {
                return io;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    twr_link::wire::write(&mut io, &Request::Status).await?;
    let Reply::Ok(status) = twr_link::wire::read(&mut io).await? else {
        anyhow::bail!("service status failed")
    };
    assert!(
        status["endpoint"].is_null(),
        "unused service published an endpoint"
    );
    drop(io);
    twr_link::bootstrap::request(&paths, &Request::SetEnabled { enabled: false }).await?;
    twr_link::bootstrap::request(&paths, &Request::Stop).await?;
    tokio::time::timeout(Duration::from_secs(10), task)
        .await?
        .context("service task failed")??;
    assert!(machine.load()?.disabled);
    assert!(!machine.socket.exists());
    Ok(())
}

/// Expensive conformance: compile this hub's embedded source in an isolated runner tree.
/// Run explicitly with --ignored; no discovery, services or agent CLIs are involved.
#[tokio::test]
#[ignore = "compiles the complete recorded twr source with one native Cargo job"]
async fn the_hubs_recorded_source_builds_as_a_native_worker() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let machine = Machine {
        dir: temp.path().join("machine"),
        socket: temp.path().join("run/sock"),
    };
    machine.init()?;
    let (source, _bundle_dir, bundle) =
        twr_link::source::export(Path::new(env!("CARGO_BIN_EXE_twr")))
            .await
            .context("exporting this hub’s recorded source")?;
    let native = twr_link::source::build(
        &machine,
        &format!("{}/{}", "ab".repeat(32), "cd".repeat(32)),
        &source,
        &bundle,
        0,
    )
    .await
    .context("building the recorded source on the native runner")?;
    assert_eq!(native.build.protocol, twr_core::runner::PROTOCOL_VERSION);
    assert!(native.build.matches(&native.executable));
    Ok(())
}

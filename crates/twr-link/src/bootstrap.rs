//! Bootstrap from the locally installed executable into an immutable service slot.
//! Remote workspace builds never select this executable. Unattended updates must go through
//! the signed updater; closing a hub does not stop this independent process.
use crate::{
    state::{atomic_write, Machine},
    wire,
};
use anyhow::{ensure, Context, Result};
use std::{path::Path, time::Duration};
use tokio::net::UnixStream;
use twr_core::paths::{ensure_private_dir, Paths};

/// Connects to the machine's runner service: at its socket, else at the `/tmp` socket of a
/// service an earlier release started ([`Machine::legacy_socket`]), which serves until it is
/// replaced (a new build stops it and starts the new one at the new socket).
pub async fn connect(machine: &Machine) -> std::io::Result<UnixStream> {
    match UnixStream::connect(&machine.socket).await {
        Ok(stream) => Ok(stream),
        Err(error) => match machine.legacy_socket() {
            Some(legacy) => UnixStream::connect(legacy).await.map_err(|_| error),
            None => Err(error),
        },
    }
}
pub async fn request(
    paths: &Paths,
    request: &crate::service::Request,
) -> Result<serde_json::Value> {
    let machine = Machine::of(paths);
    let mut stream = connect(&machine)
        .await
        .context("runner service is not running")?;
    wire::write(&mut stream, request).await?;
    let result: crate::service::Reply = tokio::time::timeout(
        Duration::from_secs(120),
        wire::read_with_timeout(&mut stream, Duration::from_secs(120)),
    )
    .await??;
    match result {
        crate::service::Reply::Ok(value) => Ok(value),
        crate::service::Reply::Error(message) => anyhow::bail!("{message}"),
    }
}
async fn status(paths: &Paths) -> Option<serde_json::Value> {
    tokio::time::timeout(
        Duration::from_secs(1),
        request(paths, &crate::service::Request::Status),
    )
    .await
    .ok()?
    .ok()
}
pub async fn ensure(paths: &Paths, binary: &Path) -> Result<()> {
    let machine = Machine::of(paths);
    machine.init()?;
    if machine.load()?.disabled {
        return Ok(());
    }
    start(paths, binary).await
}
pub async fn start(paths: &Paths, binary: &Path) -> Result<()> {
    // The shared transport service has its own lifecycle. A hub selects only its worker.
    if status(paths)
        .await
        .is_some_and(|s| s["service_protocol"] == 1)
    {
        return Ok(());
    }
    replace(paths, binary).await
}
type LaunchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
trait Launcher: Sync {
    fn start<'a>(
        &'a self,
        machine: &'a Machine,
        program: &'a Path,
        root: &'a Path,
    ) -> LaunchFuture<'a>;
}
struct SystemLauncher;
impl Launcher for SystemLauncher {
    fn start<'a>(
        &'a self,
        machine: &'a Machine,
        program: &'a Path,
        root: &'a Path,
    ) -> LaunchFuture<'a> {
        Box::pin(launch(machine, program, root))
    }
}
pub async fn replace(paths: &Paths, binary: &Path) -> Result<()> {
    replace_with_launcher(paths, binary, &SystemLauncher).await
}
async fn replace_with_launcher(
    paths: &Paths,
    binary: &Path,
    launcher: &dyn Launcher,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let machine = Machine::of(paths);
    machine.init()?;
    let expected = twr_core::build_id_of(binary)?;
    if status(paths).await.is_some_and(|v| v["build"] == expected) {
        return Ok(());
    }
    let lock = twr_core::paths::open_private_append(&machine.dir.join("bootstrap.lock"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "another client is starting the runner service"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let current = status(paths).await;
    if current.as_ref().is_some_and(|v| v["build"] == expected) {
        return Ok(());
    }
    let previous = current
        .as_ref()
        .and_then(|v| v["executable"].as_str())
        .map(std::path::PathBuf::from);
    let owned = install(&machine, binary)?;
    if current.is_some() {
        stop_existing(paths).await?;
    }
    let result = async {
        launcher
            .start(&machine, &owned, &paths.root().data_dir)
            .await?;
        ready(paths, &expected).await
    }
    .await;
    if let Err(error) = result {
        // A failed candidate cannot strand the machine's links. Only an executable already
        // running from this machine's immutable service slots is eligible for rollback.
        if let Some(previous) = previous.filter(|p| p.starts_with(machine.dir.join("service"))) {
            if status(paths)
                .await
                .is_some_and(|v| v["executable"].as_str().map(Path::new) == Some(owned.as_path()))
            {
                stop_existing(paths)
                    .await
                    .context("failed candidate could not stop for rollback")?;
            }
            let rollback = install(&machine, &previous)?;
            launcher
                .start(&machine, &rollback, &paths.root().data_dir)
                .await?;
            ready(paths, &twr_core::build_id_of(&rollback)?)
                .await
                .context("previous service could not recover after a failed update")?;
        }
        return Err(error);
    }
    Ok(())
}
async fn stop_existing(paths: &Paths) -> Result<()> {
    request(paths, &crate::service::Request::Stop).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while connect(&Machine::of(paths)).await.is_ok() {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "old runner service did not stop for maintenance"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
async fn ready(paths: &Paths, expected: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if status(paths).await.is_some_and(|v| v["build"] == expected) {
            return Ok(());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "runner service did not become healthy; see service.log"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn launch(machine: &Machine, program: &Path, root: &Path) -> Result<()> {
    use std::ffi::OsString;
    let args: Vec<OsString> = vec![
        "runner".into(),
        "service".into(),
        "--root".into(),
        root.into(),
        "--socket".into(),
        machine.socket.clone().into_os_string(),
    ];
    let forced = std::env::var_os(twr_runner::relay::LAUNCHER_ENV);
    if forced.as_deref() != Some(std::ffi::OsStr::new("setsid")) {
        #[cfg(target_os = "macos")]
        {
            use twr_runner::relay::{launchd, RelayOptions};
            let key: [u8; 32] = ring::digest::digest(
                &ring::digest::SHA256,
                machine.dir.as_os_str().as_encoded_bytes(),
            )
            .as_ref()
            .try_into()?;
            let mut opts = RelayOptions::new("runner-service", key, false)?;
            opts.program = program.into();
            opts.paths.data_dir = machine.dir.clone();
            if launchd::start(
                &opts,
                &args,
                forced.as_deref() == Some(std::ffi::OsStr::new("launchd")),
                &launchd::Launchctl::system(),
                std::env::vars_os().collect(),
                &mut |line| tracing::info!("{line}"),
            )
            .await?
            {
                return Ok(());
            }
        }
        #[cfg(target_os = "linux")]
        {
            let id = hex::encode(crate::random::<8>()?);
            if twr_runner::relay::systemd_service_run(&format!("twr-runner-{id}"), program, &args)
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
    }
    // A short-lived shell forks the service then exits, removing the original hub from
    // its ancestry. Session isolation prevents a terminal close from signalling it.
    let log = twr_core::paths::open_private_append(&machine.dir.join("service.log"))?;
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", "\"$@\" &", "twr-service-launch"])
        .arg(program)
        .args(args)
        .env_clear()
        .envs(fallback_env(std::env::vars_os()))
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting runner service")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
/// The environment of a service started in a session of its own: what launchd and
/// `systemd-run` give it ([`twr_runner::relay::launchd::service_env`]), plus two test knobs
/// that hold no secret: how it starts its workers (`TWR_BOX_LAUNCHER`, which forces this
/// start too) and whether they offer the fake adapter.
fn fallback_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let vars: Vec<_> = vars.into_iter().collect();
    let mut env = twr_runner::relay::launchd::service_env(vars.iter().cloned());
    env.extend(vars.into_iter().filter(|(name, _)| {
        name == twr_runner::relay::LAUNCHER_ENV || name == twr_adapters::ENABLE_FAKE_ENV
    }));
    env
}
fn install(machine: &Machine, binary: &Path) -> Result<std::path::PathBuf> {
    let program = copy_executable(&machine.dir.join("service"), binary)?;
    atomic_write(
        &machine.dir.join("service/current"),
        program.as_os_str().as_encoded_bytes(),
    )?;
    Ok(program)
}
/// Retain the hub's own executable independently from shared-service updates and installs.
pub fn snapshot_hub(paths: &Paths) -> Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    let source = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let source = std::env::current_exe()?;
    let machine = Machine::of(paths);
    machine.init()?;
    let binary = copy_executable(&machine.dir.join("hub-builds"), &source)?;
    let build = binary
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .context("hub build digest missing")?
        .to_string();
    let record = crate::cache::HubRef {
        pid: std::process::id(),
        start: twr_runner::reap::own_start_time(),
        build,
    };
    atomic_write(
        &machine
            .dir
            .join("hub-builds/.refs")
            .join(format!("{}.json", record.pid)),
        &serde_json::to_vec(&record)?,
    )?;
    Ok(binary)
}
fn copy_executable(base: &Path, binary: &Path) -> Result<std::path::PathBuf> {
    use std::io::{Read, Seek};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut source = std::fs::File::open(binary)?;
    ensure!(
        source.metadata()?.len() <= crate::worker::MAX_EXECUTABLE,
        "executable exceeds cache size limit"
    );
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut size = 0;
    let mut buf = [0; 65536];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ensure!(
            size <= crate::worker::MAX_EXECUTABLE,
            "executable grew beyond cache size limit"
        );
        digest.update(&buf[..n]);
    }
    let id = hex::encode(digest.finish());
    let dir = base.join(&id);
    ensure_private_dir(&dir)?;
    let destination = dir.join("twr");
    let verify = || -> Result<()> {
        let meta = std::fs::symlink_metadata(&destination)?;
        ensure!(
            meta.is_file()
                && meta.uid() == twr_core::paths::effective_uid()
                && meta.permissions().mode() & 0o077 == 0,
            "service executable must be an owner-only regular file"
        );
        ensure!(
            crate::worker::Build::of(&destination)?.sha256 == id,
            "service executable digest mismatch"
        );
        Ok(())
    };
    if std::fs::symlink_metadata(&destination).is_ok() {
        verify()?;
        return Ok(destination);
    }
    source.rewind()?;
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    let copied = std::io::copy(
        &mut (&mut source).take(crate::worker::MAX_EXECUTABLE + 1),
        &mut temporary,
    )?;
    ensure!(copied == size, "executable changed while caching");
    ensure!(
        crate::worker::Build::of(temporary.path())?.sha256 == id,
        "executable changed while caching"
    );
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => verify()?,
        Err(e) => return Err(e.error.into()),
    }
    std::fs::File::open(&dir)?.sync_all()?;
    Ok(destination)
}

pub async fn open(
    paths: &Paths,
    name: &str,
    executable: &Path,
    replace: bool,
) -> Result<(UnixStream, std::path::PathBuf, String)> {
    let mut stream = connect(&Machine::of(paths)).await?;
    wire::write(
        &mut stream,
        &crate::service::Request::Open {
            name: name.into(),
            workspace: paths.workspace_name().into(),
            executable: executable.into(),
            replace,
        },
    )
    .await?;
    let reply = wire::read_with_timeout(&mut stream, Duration::from_secs(120)).await?;
    let crate::service::Reply::Ok(value) = reply else {
        if let crate::service::Reply::Error(message) = reply {
            anyhow::bail!("{message}")
        }
        unreachable!()
    };
    let root = std::path::PathBuf::from(
        value["root"]
            .as_str()
            .context("missing managed worker root")?,
    );
    let build = value["build"]
        .as_str()
        .context("missing worker build identity")?
        .to_string();
    Ok((stream, root, build))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_detached_service_gets_the_allowlist_and_test_knobs_only() {
        let env = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("XDG_DATA_HOME", "/home/u/data"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("TWR_WORKSPACE", "work"),
            (twr_runner::relay::LAUNCHER_ENV, "setsid"),
            (twr_adapters::ENABLE_FAKE_ENV, "1"),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let mut names: Vec<_> = fallback_env(env)
            .into_iter()
            .map(|(k, _)| k.into_string().unwrap())
            .collect();
        names.sort();
        let mut expected = vec![
            "HOME",
            "PATH",
            "XDG_DATA_HOME",
            twr_runner::relay::LAUNCHER_ENV,
            twr_adapters::ENABLE_FAKE_ENV,
        ];
        expected.sort();
        assert_eq!(names, expected);
    }
    struct MockLauncher {
        good: String,
        stopped_bad: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Launcher for MockLauncher {
        fn start<'a>(
            &'a self,
            machine: &'a Machine,
            program: &'a Path,
            _root: &'a Path,
        ) -> LaunchFuture<'a> {
            Box::pin(async move {
                let actual = twr_core::build_id_of(program)?;
                let healthy = actual == self.good;
                let socket = machine.socket.clone();
                let executable = program.to_path_buf();
                let stopped = self.stopped_bad.clone();
                let listener = tokio::net::UnixListener::bind(&socket)?;
                twr_core::paths::restrict_file(&socket)?;
                tokio::spawn(async move {
                    while let Ok((mut io, _)) = listener.accept().await {
                        let Ok(request) = wire::read::<crate::service::Request>(&mut io).await
                        else {
                            continue;
                        };
                        let stop = matches!(request, crate::service::Request::Stop);
                        let value = if stop {
                            serde_json::json!({"stopping":true})
                        } else {
                            serde_json::json!({"build":if healthy{actual.clone()}else{"0000000000000000".into()},"executable":executable})
                        };
                        let _ = wire::write(&mut io, &crate::service::Reply::Ok(value)).await;
                        if stop {
                            if !healthy {
                                stopped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            break;
                        }
                    }
                    drop(listener);
                    let _ = std::fs::remove_file(socket);
                });
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn unhealthy_candidate_is_stopped_and_the_previous_owned_service_recovers() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let paths = Paths::with_home(temp.path());
        let machine = Machine::of(&paths);
        machine.init()?;
        let old = temp.path().join("old");
        let candidate = temp.path().join("bad");
        std::fs::write(&old, b"old trusted installation")?;
        std::fs::write(&candidate, b"new incompatible candidate")?;
        let owned = install(&machine, &old)?;
        let expected = twr_core::build_id_of(&owned)?;
        let launcher = MockLauncher {
            good: expected.clone(),
            stopped_bad: Default::default(),
        };
        launcher.start(&machine, &owned, &paths.data_dir).await?;
        let work = machine.dir.join("runner/work");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("result"), b"unique work")?;
        assert!(replace_with_launcher(&paths, &candidate, &launcher)
            .await
            .is_err());
        assert_eq!(
            status(&paths)
                .await
                .context("rollback did not restore a service")?["build"],
            expected
        );
        assert_eq!(
            launcher
                .stopped_bad
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            crate::state::read_private(&machine.dir.join("service/current"), 4096)?,
            owned.as_os_str().as_encoded_bytes()
        );
        assert_eq!(std::fs::read(work.join("result"))?, b"unique work");
        stop_existing(&paths).await?;
        Ok(())
    }
}

//! Native worker builds use immutable materializations and one machine-wide build lease.
use crate::{bundle::Repository, state::Machine, worker::Build};
use anyhow::{ensure, Context, Result};
use std::{
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};
pub use twr_source::Source;
/// How often a running native build re-checks the free-space reserve and owner budget. The
/// build's own compiler scratch is not charged to the budget ([`twr_source::budget_bytes`]).
const BUILD_BUDGET_CHECK: Duration = Duration::from_secs(15);
pub fn host_target() -> &'static str {
    twr_core::build_target()
}
fn rustup() -> PathBuf {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let path = PathBuf::from(home).join("bin/rustup");
        if path.is_file() {
            return path;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".cargo/bin/rustup");
        if path.is_file() {
            return path;
        }
    }
    "rustup".into()
}
pub async fn native_target(compiler: &str) -> Result<String> {
    let mut probe = tokio::process::Command::new(rustup());
    probe.args(["run", compiler, "rustc", "-vV"]);
    let result =
        twr_adapters::process::run_bounded(probe, Duration::from_secs(10), 8192, |_| false).await;
    let twr_adapters::process::Bounded::Exited(Some(0), stdout) = result else {
        anyhow::bail!("install Rust {compiler} on the runner to build this hub's recorded source")
    };
    let output = std::str::from_utf8(&stdout)?;
    let target = output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("native compiler did not report a target")?;
    ensure!(
        target.starts_with(std::env::consts::ARCH)
            && match std::env::consts::OS {
                "macos" => target.ends_with("apple-darwin"),
                "linux" => target.contains("-linux-"),
                _ => false,
            },
        "installed compiler host does not match the runner platform"
    );
    Ok(target.to_string())
}
pub async fn export(executable: &Path) -> Result<(Source, tempfile::TempDir, PathBuf)> {
    let mut cmd = tokio::process::Command::new(executable);
    cmd.arg("--export-worker-source");
    let output = twr_adapters::process::run_bounded(
        cmd,
        Duration::from_secs(30),
        crate::bundle::MAX_BUNDLE as usize + crate::wire::MAX_CONTROL + 4,
        |_| false,
    )
    .await;
    let twr_adapters::process::Bounded::Exited(Some(0), bytes) = output else {
        anyhow::bail!(
            "this hub build has no recorded source; rebuild with linked-worker source capture"
        )
    };
    ensure!(bytes.len() >= 4, "source export is truncated");
    let len = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
    ensure!(
        len <= crate::wire::MAX_CONTROL && bytes.len() >= 4 + len,
        "source manifest exceeds limit"
    );
    let source: Source = serde_json::from_slice(&bytes[4..4 + len])?;
    source.validate()?;
    let payload = &bytes[4 + len..];
    ensure!(
        payload.len() as u64 == source.bundle.size,
        "source bundle size mismatch"
    );
    ensure!(
        hex::encode(ring::digest::digest(&ring::digest::SHA256, payload)) == source.bundle.sha256,
        "source bundle digest mismatch"
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.bundle");
    crate::state::atomic_write(&path, payload)?;
    Ok((source, directory, path))
}
pub struct Native {
    pub build: Build,
    pub executable: PathBuf,
    pub output: PathBuf,
    pub cleanup: Option<PathBuf>,
    _lease: std::fs::File,
}
pub async fn cached(machine: &Machine, scope: &str, source: &Source) -> Result<Option<Build>> {
    let Ok(target) = native_target(&source.compiler).await else {
        return Ok(None);
    };
    let output = machine
        .dir
        .join("runner/hubs")
        .join(scope)
        .join("native")
        .join(source.id(&target)?)
        .join("output.json");
    if let Ok(bytes) = crate::state::read_private(&output, 4096) {
        let build: Build = serde_json::from_slice(&bytes)?;
        if build.matches(&build.path(machine, scope)) {
            return Ok(Some(build));
        }
    }
    Ok(None)
}
pub async fn build(
    machine: &Machine,
    scope: &str,
    source: &Source,
    bundle: &Path,
    epoch: u64,
) -> Result<Native> {
    source.validate()?;
    let resources = machine.dir.join("runner/leases");
    twr_core::paths::ensure_private_dir(&resources)?;
    let gate = twr_runner::managed::try_admission_gate(&resources)?
        .context("runner admission is busy; retry the native build")?;
    let lease = twr_core::paths::open_private_append(&resources.join("build.lock"))?;
    ensure!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another native worker build is running; retry when it finishes"
    );
    drop(gate);
    let marker = format!("twr-linked-build:{}", hex::encode(crate::random::<32>()?));
    let record = BuildRecord {
        owner: scope
            .split('/')
            .next()
            .context("source owner missing")?
            .into(),
        epoch,
        marker: marker.clone(),
        pid: std::process::id(),
        start: twr_runner::reap::own_start_time(),
    };
    crate::state::atomic_write(&resources.join("build.json"), &serde_json::to_vec(&record)?)?;
    twr_runner::managed::disk_admission(&machine.dir)?;
    let target = match native_target(&source.compiler).await {
        Ok(target) => target,
        Err(_) => {
            let mut install = tokio::process::Command::new(rustup());
            install
                .args([
                    "toolchain",
                    "install",
                    &source.compiler,
                    "--profile",
                    "minimal",
                ])
                .env(twr_runner::reap::AGENT_ENV, &marker);
            inherit_build_lease(&mut install, &lease);
            ensure!(
                matches!(
                    twr_adapters::process::run_bounded(
                        install,
                        Duration::from_secs(600),
                        65536,
                        |_| false
                    )
                    .await,
                    twr_adapters::process::Bounded::Exited(Some(0), _)
                ),
                "install rustup on the runner to provision the recorded Rust compiler"
            );
            native_target(&source.compiler).await?
        }
    };
    let id = source.id(&target)?;
    let root = machine.dir.join("runner/hubs").join(scope);
    twr_core::paths::ensure_private_dir(&root)?;
    let owner_root = root.parent().context("source scope has no owner")?;
    crate::bundle::reserve(owner_root, source.bundle.size).await?;
    let cache = root.join("native").join(id);
    twr_core::paths::ensure_private_dir(&cache)?;
    let output = cache.join("output.json");
    if let Ok(bytes) = crate::state::read_private(&output, 4096) {
        let build: Build = serde_json::from_slice(&bytes)?;
        if build.matches(&build.path(machine, scope)) {
            return Ok(Native {
                executable: build.path(machine, scope),
                build,
                output,
                cleanup: None,
                _lease: lease,
            });
        }
    }
    let repository =
        Repository::init_format(&cache.join("objects"), &source.bundle.commit, true).await?;
    repository
        .receive(bundle, &source.bundle, "refs/twr/source")
        .await?;
    let materialized = cache.join("source");
    // Failed clones never become reusable materializations.
    if !materialized.exists() {
        let staging = tempfile::tempdir_in(&cache)?;
        let checkout = staging.path().join("source");
        let mut cmd = crate::bundle::command(&cache);
        cmd.args(["clone", "--no-hardlinks", "--no-checkout"])
            .arg(&repository.0)
            .arg(&checkout);
        crate::bundle::run(cmd).await?;
        Repository(checkout.clone())
            .git(&["checkout", "--detach", &source.bundle.commit])
            .await?;
        std::fs::rename(checkout, &materialized)?;
    }
    verify_materialization(&materialized, &source.bundle.commit).await?;
    let saved_bundle = cache.join("source.bundle");
    std::fs::copy(bundle, &saved_bundle)?;
    let manifest = cache.join("source.json");
    crate::state::atomic_write(&manifest, &serde_json::to_vec(source)?)?;
    // rustup run never installs a missing compiler (unlike cargo +toolchain).
    let mut probe = tokio::process::Command::new(rustup());
    probe.args(["run", &source.compiler, "rustc", "--version"]);
    let probe =
        twr_adapters::process::run_bounded(probe, Duration::from_secs(10), 4096, |_| false).await;
    ensure!(
        matches!(probe, twr_adapters::process::Bounded::Exited(Some(0), _)),
        "install Rust {} on the runner to build this hub's recorded source",
        source.compiler
    );
    let target_dir = cache.join("target");
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args([
        "-c",
        "exec \"$@\" 2>&1",
        "twr-native-build",
        "nice",
        "-n",
        "10",
    ])
    .arg(rustup())
    .args([
        "run",
        &source.compiler,
        "cargo",
        "build",
        "--quiet",
        "--jobs",
        "1",
        "--locked",
        "-p",
        "twr-cli",
        "--target",
        &target,
    ])
    .current_dir(&materialized);
    if source.profile == "release" {
        cmd.arg("--release");
    }
    let keep = [
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TMPDIR",
        "LANG",
    ];
    let environment: Vec<_> = keep
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (*key, value)))
        .collect();
    cmd.env_clear()
        .envs(environment)
        .env(twr_runner::reap::AGENT_ENV, &marker)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_ENCODED_RUSTFLAGS", source.flags.join("\x1f"))
        .env("TWR_WORKER_SOURCE_BUNDLE", &saved_bundle)
        .env("TWR_WORKER_SOURCE_MANIFEST", &manifest);
    inherit_build_lease(&mut cmd, &lease);
    let run =
        twr_adapters::process::run_bounded(cmd, Duration::from_secs(30 * 60), 64 * 1024, |_| false);
    tokio::pin!(run);
    loop {
        tokio::select! {result=&mut run=>{match result {twr_adapters::process::Bounded::Exited(Some(0),_)=>{},twr_adapters::process::Bounded::Exited(_,bytes)=>{let tail=&bytes[bytes.len().saturating_sub(4096)..];anyhow::bail!("native worker build failed: {}",String::from_utf8_lossy(tail));},_=>anyhow::bail!("native worker build could not complete within its time/output limit; check the runner's toolchain and dependencies")};break},_=tokio::time::sleep(BUILD_BUDGET_CHECK)=>{crate::bundle::reserve(owner_root,0).await?;}}
    }
    verify_materialization(&materialized, &source.bundle.commit).await?;
    let executable = target_dir.join(&target).join(&source.profile).join("twr");
    let build = Build::executable(&executable).await?;
    Ok(Native {
        build,
        executable,
        output,
        cleanup: Some(target_dir),
        _lease: lease,
    })
}

async fn verify_materialization(path: &Path, commit: &str) -> Result<()> {
    let snapshot = Repository(path.to_path_buf()).snapshot().await;
    if snapshot.as_ref().is_ok_and(|actual| actual == commit) {
        return Ok(());
    }
    std::fs::remove_dir_all(path).context("remove changed native source materialization")?;
    snapshot?;
    anyhow::bail!("native source materialization changed; retry to restore the recorded source")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BuildRecord {
    owner: String,
    epoch: u64,
    marker: String,
    pid: u32,
    start: Option<u64>,
}
fn inherit_build_lease(command: &mut tokio::process::Command, lease: &std::fs::File) {
    let fd = lease.as_raw_fd();
    // Change only the forked child's descriptor flags. Unrelated service children retain
    // CLOEXEC, while Cargo and its compiler descendants keep the single-build kernel lease.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
fn build_record(machine: &Machine) -> Result<Option<BuildRecord>> {
    match crate::state::read_private(&machine.dir.join("runner/leases/build.json"), 4096) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
fn retire_record(record: &BuildRecord) -> Result<()> {
    let markers = vec![record.marker.clone()];
    let _ = twr_runner::reap::reap_agent_marked(&markers);
    ensure!(
        twr_runner::reap::agent_markers_left(&markers).is_some_and(|left| left.is_empty()),
        "native build processes could not be retired"
    );
    Ok(())
}
pub fn retire_orphan(machine: &Machine) -> Result<()> {
    if let Some(record) = build_record(machine)? {
        if twr_runner::reap::process_liveness(record.pid, record.start)
            == twr_runner::reap::Alive::No
        {
            retire_record(&record)?;
        }
    }
    Ok(())
}
pub fn retire_grant(machine: &Machine, owner: &str, epoch: u64) -> Result<()> {
    if let Some(record) = build_record(machine)? {
        if record.owner == owner && record.epoch == epoch {
            retire_record(&record)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cargo_child_keeps_the_build_lease_without_leaking_it_to_other_children() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("build.lock");
        let lease = twr_core::paths::open_private_append(&path)?;
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let mut command = tokio::process::Command::new("sleep");
        command.arg("30").kill_on_drop(true);
        inherit_build_lease(&mut command, &lease);
        let mut child = command.spawn()?;
        let mut unrelated = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()?;
        drop(lease);
        let probe = twr_core::paths::open_private_append(&path)?;
        assert_ne!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        child.kill().await?;
        child.wait().await?;
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(unrelated.try_wait()?.is_none());
        unrelated.kill().await?;
        unrelated.wait().await?;
        Ok(())
    }
    #[tokio::test]
    async fn crashed_build_cleanup_is_scoped_to_its_recorded_marker_and_epoch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let machine = Machine {
            dir: temp.path().join("machine"),
            socket: temp.path().join("run/sock"),
        };
        machine.init()?;
        let marker = format!("twr-test-build:{}", hex::encode(crate::random::<16>()?));
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .env(twr_runner::reap::AGENT_ENV, &marker)
            .kill_on_drop(true)
            .spawn()?;
        let mut unrelated = tokio::process::Command::new("sleep")
            .arg("30")
            .env(twr_runner::reap::AGENT_ENV, format!("{marker}-other"))
            .kill_on_drop(true)
            .spawn()?;
        // ps can reject an out-of-range sentinel PID instead of reporting it gone.
        // Record a real creator and wait for its exit, as a crashed service would.
        let mut creator = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()?;
        let creator_pid = creator.id().context("creator PID missing")?;
        let creator_start = twr_runner::reap::process_start_time(creator_pid)
            .context("creator start time missing")?;
        creator.kill().await?;
        creator.wait().await?;
        let record = BuildRecord {
            owner: "ab".repeat(32),
            epoch: 4,
            marker,
            pid: creator_pid,
            start: Some(creator_start),
        };
        assert_eq!(
            twr_runner::reap::process_liveness(record.pid, record.start),
            twr_runner::reap::Alive::No
        );
        crate::state::atomic_write(
            &machine.dir.join("runner/leases/build.json"),
            &serde_json::to_vec(&record)?,
        )?;
        retire_grant(&machine, &record.owner, 5)?;
        assert!(child.try_wait()?.is_none());
        retire_orphan(&machine)?;
        assert!(child.try_wait()?.is_some());
        assert!(unrelated.try_wait()?.is_none());
        unrelated.kill().await?;
        unrelated.wait().await?;
        Ok(())
    }
    #[tokio::test]
    async fn changed_materialization_is_removed_before_retry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        std::fs::create_dir(&source)?;
        let repo = Repository(source.clone());
        repo.git(&["init", "."]).await?;
        std::fs::write(source.join("data"), "recorded")?;
        let commit = repo.snapshot().await?;
        std::fs::write(source.join("data"), "changed by a build script")?;
        assert!(verify_materialization(&source, &commit).await.is_err());
        assert!(!source.exists());
        Ok(())
    }
    #[tokio::test]
    async fn recorded_archive_builds_with_the_pinned_native_compiler() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let checkout = temp.path().join("archive");
        std::fs::create_dir(&checkout)?;
        std::fs::write(checkout.join("Cargo.toml"),"[package]\nname=\"twr-cli\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[[bin]]\nname=\"twr\"\npath=\"main.rs\"\n")?;
        std::fs::write(checkout.join("Cargo.lock"),"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"twr-cli\"\nversion = \"0.1.0\"\n")?;
        std::fs::write(
            checkout.join("main.rs"),
            r##"fn main(){println!("{}",r#"{"worker_protocol":7,"worker_features":["managed_paths_v1","global_leases_v1","revocation_v1"]}"#);}"##,
        )?;
        let bundle_path = temp.path().join("source.bundle");
        let bundle = Repository(checkout).capture_bundle(&bundle_path).await?;
        let rustc = std::process::Command::new("rustc")
            .arg("--version")
            .output()?;
        ensure!(rustc.status.success(), "compiler unavailable");
        let compiler = String::from_utf8(rustc.stdout)?
            .split_whitespace()
            .nth(1)
            .context("compiler release missing")?
            .to_string();
        if native_target(&compiler).await.is_err() {
            eprintln!("skipping compiler fixture: the PATH compiler is not installed in rustup; tests never install toolchains");
            return Ok(());
        }
        let source = Source {
            bundle,
            compiler,
            profile: "debug".into(),
            flags: Vec::new(),
        };
        let machine = Machine {
            dir: temp.path().join("machine"),
            socket: temp.path().join("run/sock"),
        };
        machine.init()?;
        let native = build(
            &machine,
            &format!("{}/{}", "ab".repeat(32), "cd".repeat(32)),
            &source,
            &bundle_path,
            0,
        )
        .await?;
        assert_eq!(native.build.protocol, 7);
        assert!(native.build.matches(&native.executable));
        Ok(())
    }
}

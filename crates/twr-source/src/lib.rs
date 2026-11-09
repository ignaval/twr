//! Credential-free exact-commit transfer. No origin fetches, global programs, or checkout edits.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub bundle: Bundle,
    pub compiler: String,
    pub profile: String,
    pub flags: Vec<String>,
}
impl Source {
    pub fn validate(&self) -> Result<()> {
        self.bundle.validate()?;
        ensure!(
            self.compiler.len() <= 32
                && self
                    .compiler
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.'),
            "native worker requires a stable compiler release"
        );
        ensure!(
            matches!(self.profile.as_str(), "debug" | "release")
                && self.flags.len() <= 64
                && self
                    .flags
                    .iter()
                    .all(|f| f.len() <= 4096 && !f.contains('\0')),
            "unsupported source build options"
        );
        Ok(())
    }
    pub fn id(&self, target: &str) -> Result<String> {
        let mut bytes = serde_json::to_vec(&(
            &self.bundle.commit,
            &self.compiler,
            &self.profile,
            &self.flags,
        ))?;
        bytes.extend(target.as_bytes());
        Ok(hex::encode(ring::digest::digest(
            &ring::digest::SHA256,
            &bytes,
        )))
    }
}

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use twr_adapters::process::{run_bounded, Bounded};
pub const MAX_BUNDLE: u64 = 128 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 4 * 1024 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub commit: String,
    pub source_ref: String,
    pub sha256: String,
    pub size: u64,
}
impl Bundle {
    pub fn validate(&self) -> Result<()> {
        ensure!(valid_oid(&self.commit), "invalid commit id");
        ensure!(
            self.source_ref.starts_with("refs/twr/sync/")
                && self.source_ref[14..].bytes().all(|b| b.is_ascii_hexdigit())
                && self.source_ref.len() == 46,
            "invalid bundle ref"
        );
        ensure!(
            self.size > 0 && self.size <= MAX_BUNDLE,
            "bundle exceeds transfer limit"
        );
        ensure!(
            self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid bundle digest"
        );
        Ok(())
    }
}
pub fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn project_id(name: &str) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, name.as_bytes()))
}
pub fn command(repo: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_")
            || key == "SSH_ASKPASS"
            || key == "SSH_AUTH_SOCK"
        {
            cmd.env_remove(key);
        }
    }
    cmd.envs([
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ALLOW_PROTOCOL", "file"),
        ("GIT_NO_LAZY_FETCH", "1"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_LFS_SKIP_SMUDGE", "1"),
        ("GIT_AUTHOR_NAME", "twr source"),
        ("GIT_AUTHOR_EMAIL", "source@example.invalid"),
        ("GIT_COMMITTER_NAME", "twr source"),
        ("GIT_COMMITTER_EMAIL", "source@example.invalid"),
        ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+0000"),
        ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+0000"),
    ]);
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgSign=false",
        "tag.gpgSign=false",
        "gc.auto=0",
        "maintenance.auto=false",
        "protocol.allow=never",
        "protocol.file.allow=always",
        "credential.helper=",
    ] {
        cmd.arg("-c").arg(setting);
    }
    // A malicious pack cannot make one unbounded output file. Aggregate growth is monitored.
    unsafe {
        cmd.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: MAX_UNPACKED,
                rlim_max: MAX_UNPACKED,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}
pub async fn run(cmd: tokio::process::Command) -> Result<Vec<u8>> {
    match run_bounded(cmd, Duration::from_secs(300), 1024 * 1024, |_| false).await {
        Bounded::Exited(Some(0), stdout) => Ok(stdout),
        _ => anyhow::bail!("Git operation failed or exceeded its five-minute/output limit"),
    }
}
#[derive(Clone)]
pub struct Repository(pub PathBuf);
impl Repository {
    pub async fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = command(&self.0);
        cmd.args(args);
        run(cmd).await
    }
    pub async fn text(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8(self.git(args).await?)?
            .trim_end_matches('\n')
            .to_string())
    }
    pub async fn resolve(&self, value: &str) -> Result<String> {
        ensure!(
            !value.starts_with('-') && value.len() <= 256,
            "invalid commit reference"
        );
        let oid = self
            .text(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{value}^{{commit}}"),
            ])
            .await?;
        ensure!(valid_oid(&oid), "Git returned an invalid commit");
        Ok(oid)
    }
    pub async fn init_format(path: &Path, oid: &str, bare: bool) -> Result<Self> {
        ensure!(valid_oid(oid), "invalid commit id");
        twr_core::paths::ensure_private_dir(path)?;
        let repo = Self(path.to_path_buf());
        let format = if oid.len() == 64 {
            "--object-format=sha256"
        } else {
            "--object-format=sha1"
        };
        if bare {
            repo.git(&["init", "--bare", format, "."]).await?;
        } else {
            repo.git(&["init", format, "."]).await?;
        }
        Ok(repo)
    }
    pub async fn init(path: &Path) -> Result<Self> {
        twr_core::paths::ensure_private_dir(path)?;
        let repo = Self(path.to_path_buf());
        repo.git(&["init", "--bare", "."]).await?;
        Ok(repo)
    }
    pub async fn create_bundle(
        &self,
        commit: &str,
        path: &Path,
        tips: &[String],
    ) -> Result<Bundle> {
        ensure!(
            valid_oid(commit) && tips.len() <= 64,
            "invalid transfer request"
        );
        ensure!(
            self.resolve(commit).await? == commit,
            "wanted object is not a commit"
        );
        let name = format!("refs/twr/sync/{}", hex::encode(crate::random::<16>()?));
        self.git(&["update-ref", &name, commit]).await?;
        let result = async {
            let mut cmd = command(&self.0);
            cmd.args(["bundle", "create"]).arg(path).arg(&name);
            unsafe {
                cmd.pre_exec(|| {
                    let limit = libc::rlimit {
                        rlim_cur: MAX_BUNDLE,
                        rlim_max: MAX_BUNDLE,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            for tip in tips {
                if valid_oid(tip) && self.resolve(tip).await.is_ok() {
                    cmd.arg(format!("^{tip}"));
                }
            }
            run(cmd).await?;
            let build = blob(path)?;
            ensure!(build.size <= MAX_BUNDLE, "bundle exceeds transfer limit");
            Ok(Bundle {
                commit: commit.into(),
                source_ref: name.clone(),
                sha256: build.sha256,
                size: build.size,
            })
        }
        .await;
        let _ = self.git(&["update-ref", "-d", &name]).await;
        result
    }
    pub async fn receive(&self, path: &Path, bundle: &Bundle, destination: &str) -> Result<()> {
        bundle.validate()?;
        ensure!(
            destination.starts_with("refs/twr/")
                && destination
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_-".contains(&b))
                && !destination.contains(".."),
            "invalid destination reference"
        );
        let mut cmd = command(&self.0);
        cmd.args(["bundle", "list-heads"]).arg(path);
        let heads = String::from_utf8(run(cmd).await?)?;
        ensure!(
            heads
                .lines()
                .any(|line| line == format!("{} {}", bundle.commit, bundle.source_ref)),
            "bundle does not advertise wanted commit"
        );
        let mut cmd = command(&self.0);
        cmd.args(["bundle", "verify"]).arg(path);
        run(cmd).await?;
        // Import to a temporary ref first. Publishing the result never moves the checkout.
        let temporary = format!("refs/twr/sync/{}", hex::encode(crate::random::<16>()?));
        let objects = PathBuf::from(
            self.text(&[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ])
            .await?,
        );
        let quota = directory_bytes(&objects, u64::MAX)?.saturating_add(MAX_UNPACKED);
        let result=async {
            let mut cmd=command(&self.0);cmd.args(["-c","fetch.fsckObjects=true","-c","transfer.fsckObjects=true","fetch","--no-tags","--no-write-fetch-head"]).arg(path).arg(format!("{}:{temporary}",bundle.source_ref));
            let import=run(cmd);
            tokio::pin!(import);
            loop {tokio::select!{result=&mut import=>{result?;break},_=tokio::time::sleep(Duration::from_secs(1))=>{ensure!(directory_bytes(&objects,quota)?<=quota,"unpacked objects exceed quota");}}}
            ensure!(directory_bytes(&objects,quota)?<=quota,"unpacked objects exceed quota");
            ensure!(self.resolve(&temporary).await?==bundle.commit,"imported commit differs from requested commit");
            self.git(&["update-ref",destination,&bundle.commit]).await?;Ok(())
        }.await;
        let _ = self.git(&["update-ref", "-d", &temporary]).await;
        result
    }
    async fn snapshot_in(
        &self,
        store_in_source: bool,
    ) -> Result<(tempfile::TempDir, Repository, String)> {
        let top = self
            .text(&["rev-parse", "--show-toplevel"])
            .await
            .ok()
            .and_then(|p| std::fs::canonicalize(p).ok());
        let original = if top.as_deref() == Some(std::fs::canonicalize(&self.0)?.as_path()) {
            self.text(&["rev-parse", "--absolute-git-dir"]).await.ok()
        } else {
            None
        };
        ensure!(
            original.is_some() || !self.0.join(".git").try_exists()?,
            "source Git metadata cannot be read"
        );
        ensure!(
            !store_in_source || original.is_some(),
            "snapshot requires Git metadata"
        );
        let indexed = if original.is_some() {
            self.git(&["ls-files", "-v", "-z"]).await?
        } else {
            Vec::new()
        };
        for entry in indexed.split(|b| *b == 0) {
            if let Some(first) = entry.first() {
                ensure!(
                    *first != b'S' && !first.is_ascii_lowercase(),
                    "source snapshot refuses hidden index flags"
                );
            }
        }
        let mut overrides: Vec<OsString> = Vec::new();
        // Read only the user's ignore path with their normal config, never their programs.
        let mut config = tokio::process::Command::new("git");
        config.arg("-C").arg(&self.0).args([
            "config",
            "--includes",
            "--path",
            "--get",
            "core.excludesFile",
        ]);
        let ignored = run_bounded(config, Duration::from_secs(10), 16384, |_| false).await;
        let ignored = match ignored {
            Bounded::Exited(Some(0), bytes) => Some(bytes),
            Bounded::Exited(Some(1), _) => None,
            _ => anyhow::bail!("cannot read Git ignore configuration; refusing source capture"),
        };
        if let Some(stdout) = ignored {
            let path = PathBuf::from(String::from_utf8(stdout)?.trim_end_matches('\n'));
            let path = if path.is_absolute() {
                path
            } else {
                self.0.join(path)
            };
            overrides.extend([
                "-c".into(),
                format!("core.excludesFile={}", path.display()).into(),
            ]);
        }
        let filters = self
            .git(&["config", "--null", "--list", "--includes"])
            .await?;
        for entry in filters.split(|b| *b == 0) {
            let key = entry.split(|b| *b == b'\n').next().unwrap_or_default();
            let key = std::str::from_utf8(key)?;
            if key.to_ascii_lowercase().starts_with("filter.") {
                ensure!(!key.contains(['=', '\r', '\n']), "invalid filter name");
                if ["clean", "smudge", "process"]
                    .iter()
                    .any(|last| key.to_ascii_lowercase().ends_with(&format!(".{last}")))
                {
                    overrides.extend([
                        "-c".into(),
                        format!("{key}=").into(),
                        "-c".into(),
                        format!("{}.required=false", key.rsplit_once('.').unwrap().0).into(),
                    ]);
                }
            }
        }
        let temp = tempfile::tempdir()?;
        let isolated = temp.path().join("git");
        twr_core::paths::ensure_private_dir(&isolated)?;
        let format = if original.is_some() {
            self.text(&["rev-parse", "--show-object-format"]).await?
        } else {
            "sha1".into()
        };
        let sandbox = Repository(isolated.clone());
        sandbox
            .git(&["init", "--bare", &format!("--object-format={format}"), "."])
            .await?;
        // info/attributes wins over every repository attribute, preserving raw source bytes
        // even with text normalization, encodings or filters configured by the checkout.
        std::fs::write(
            isolated.join("info/attributes"),
            "* -text -filter -working-tree-encoding\n",
        )?;
        let mut objects = isolated.join("objects");
        let index = temp.path().join("index");
        if original.is_some() {
            let local_ignore = self
                .text(&[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "info/exclude",
                ])
                .await?;
            match std::fs::copy(local_ignore, isolated.join("info/exclude")) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let original_objects = self
                .text(&[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects",
                ])
                .await?;
            std::fs::write(
                isolated.join("objects/info/alternates"),
                format!("{original_objects}\n"),
            )?;
            if store_in_source {
                objects = original_objects.into();
            }
            // Expand only a private copy of the index; tracked ignored additions and split
            // indexes stay represented without changing the user's real index.
            let original_index = self
                .text(&["rev-parse", "--path-format=absolute", "--git-path", "index"])
                .await?;
            match std::fs::copy(original_index, &index) {
                Ok(_) => {
                    let mut cmd = command(&self.0);
                    cmd.args(["update-index", "--no-split-index"])
                        .env("GIT_INDEX_FILE", &index);
                    run(cmd).await?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut cmd = command(&self.0);
        cmd.args(&overrides)
            .args(["add", "-A", "--", "."])
            .env("GIT_DIR", &isolated)
            .env("GIT_WORK_TREE", &self.0)
            .env("GIT_OBJECT_DIRECTORY", &objects)
            .env("GIT_INDEX_FILE", &index);
        run(cmd).await?;
        let mut cmd = command(&self.0);
        cmd.args(&overrides)
            .arg("write-tree")
            .env("GIT_DIR", &isolated)
            .env("GIT_WORK_TREE", &self.0)
            .env("GIT_OBJECT_DIRECTORY", &objects)
            .env("GIT_INDEX_FILE", temp.path().join("index"));
        let tree = String::from_utf8(run(cmd).await?)?.trim().to_string();
        // Parentless source snapshots need only the exact tree's objects, not all history.
        let repository = if store_in_source {
            self.clone()
        } else {
            sandbox
        };
        let commit = repository
            .text(&["commit-tree", &tree, "-m", "twr immutable source snapshot"])
            .await?;
        Ok((temp, repository, commit))
    }
    pub async fn snapshot(&self) -> Result<String> {
        let (_temp, repo, commit) = self.snapshot_in(true).await?;
        repo.git(&["update-ref", &format!("refs/twr/builds/{commit}"), &commit])
            .await?;
        Ok(commit)
    }
    /// Build-time capture writes objects and refs only in a temporary Git directory.
    pub async fn capture_bundle(&self, path: &Path) -> Result<Bundle> {
        let (_temp, repo, commit) = self.snapshot_in(false).await?;
        repo.create_bundle(&commit, path, &[]).await
    }
}
pub fn directory_bytes(root: &Path, cap: u64) -> Result<u64> {
    walk_bytes(root, cap, |_| true)
}

/// The per-owner runner budget for transfers and builds.
pub const OWNER_BUDGET: u64 = 10 * 1024 * 1024 * 1024;

/// Whether `relative` (a path below an owner's `runner/hubs/<machine>` directory) holds
/// transfer or build data the owner budget counts. Laid out as `<workspace>/builds`,
/// `<workspace>/native/<id>/…`, `<workspace>/native-incoming` and
/// `<workspace>/boxes/<box>/repos`, plus result-export scratch at the top. Not counted:
/// the rest of a box's data directory (agent worktrees, trash, spool, logs), whose growth is
/// agent work rather than what a hub transferred, and a native build's compiler scratch
/// (`native/<id>/target`), which is removed after publication and bounded by the build's time
/// limit and the free-space reserve instead.
fn budgeted(relative: &Path) -> bool {
    let parts: Vec<_> = relative.components().map(|c| c.as_os_str()).collect();
    match parts.as_slice() {
        [_, boxes, _, entry, ..] if *boxes == "boxes" => *entry == "repos",
        [_, native, _, entry, ..] if *native == "native" => *entry != "target",
        _ => true,
    }
}

/// The bytes an owner's runner directory `root` charges to its budget ([`budgeted`]), or a
/// value above `cap` once that is exceeded.
pub fn budget_bytes(root: &Path, cap: u64) -> Result<u64> {
    walk_bytes(root, cap, |path| {
        path.strip_prefix(root).map_or(true, budgeted)
    })
}

fn walk_bytes(root: &Path, cap: u64, counted: impl Fn(&Path) -> bool) -> Result<u64> {
    let mut stack = vec![root.to_path_buf()];
    let mut size = 0u64;
    let mut entries = 0usize;
    while let Some(path) = stack.pop() {
        let listing = match std::fs::read_dir(path) {
            Ok(value) => value,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in listing {
            let entry = match entry {
                Ok(value) => value,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let path = entry.path();
            if !counted(&path) {
                continue;
            }
            entries += 1;
            ensure!(
                entries < 1_000_000,
                "managed directory has too many entries"
            );
            let kind = match entry.file_type() {
                Ok(value) => value,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let metadata = match entry.metadata() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                size = size.saturating_add(metadata.len());
                if size > cap {
                    return Ok(size);
                }
            }
        }
    }
    Ok(size)
}

/// Admits `additional` transfer/build bytes for the owner whose runner directory is `root`:
/// the filesystem keeps its free-space reserve and the owner stays within [`OWNER_BUDGET`]
/// ([`budget_bytes`]). The walk runs on a blocking thread.
pub async fn reserve(root: &Path, additional: u64) -> Result<()> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || reserve_blocking(&root, additional)).await?
}

#[allow(clippy::unnecessary_cast)] // statvfs field widths differ on supported Unix targets.
pub fn reserve_blocking(root: &Path, additional: u64) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "cannot inspect runner free space"
    );
    let stat = unsafe { stat.assume_init() };
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
    let reserve = (5 * 1024 * 1024 * 1024u64).max(total / 10);
    ensure!(
        free >= reserve.saturating_add(additional),
        "runner free-space reserve reached; reclaim disk space before admitting work"
    );
    ensure!(
        budget_bytes(root, OWNER_BUDGET)?.saturating_add(additional) <= OWNER_BUDGET,
        "this hub's runner transfer/build budget is full; run `twr runner prune` on the runner"
    );
    Ok(())
}

fn random<const N: usize>() -> Result<[u8; N]> {
    use ring::rand::SecureRandom;
    let mut bytes = [0; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    Ok(bytes)
}
struct Blob {
    sha256: String,
    size: u64,
}
fn blob(path: &Path) -> Result<Blob> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(size <= MAX_BUNDLE, "bundle exceeds transfer limit");
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(Blob {
        size,
        sha256: hex::encode(hash.finish()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_measurements_tolerate_compiler_scratch_churn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let churn = root.join("scratch");
        let writer = std::thread::spawn(move || {
            for _ in 0..1000 {
                if std::fs::create_dir(&churn).is_ok() {
                    let _ = std::fs::write(churn.join("output"), [0; 100]);
                    let _ = std::fs::remove_dir_all(&churn);
                }
            }
        });
        for _ in 0..1000 {
            assert!(directory_bytes(&root, 1024 * 1024)? <= 1024 * 1024);
        }
        writer.join().unwrap();
        Ok(())
    }
    #[test]
    fn owner_budget_counts_transfers_and_builds_but_not_agent_work() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let owner = temp.path();
        let sparse = |path: &Path, len: u64| -> Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::File::create(path)?.set_len(len)?;
            Ok(())
        };
        let ws = owner.join("cd".repeat(16));
        // Agent work: one worktree's Rust target alone exceeds the budget.
        sparse(
            &ws.join("boxes/desk/worktrees/api/target/debug/big"),
            OWNER_BUDGET + 1,
        )?;
        sparse(&ws.join("boxes/desk/trash/old/blob"), OWNER_BUDGET)?;
        sparse(&ws.join("boxes/desk/runner-desk.db"), 1 << 30)?;
        for i in 0..2000 {
            sparse(
                &ws.join(format!(
                    "boxes/desk/worktrees/web/node_modules/m{}/f{i}",
                    i % 50
                )),
                1,
            )?;
        }
        // The in-progress build's compiler scratch.
        sparse(&ws.join("native/abc/target/debug/twr"), OWNER_BUDGET)?;
        assert_eq!(budget_bytes(owner, OWNER_BUDGET)?, 0);
        // Transfers and builds.
        sparse(&ws.join("boxes/desk/repos/p/.git/objects/pack/a.pack"), 100)?;
        sparse(&ws.join("builds/ef/twr"), 20)?;
        sparse(&ws.join("native/abc/objects/pack/b.pack"), 3)?;
        sparse(&ws.join("native-incoming/source.bundle"), 4)?;
        sparse(&owner.join(".tmpexport/results.bundle"), 5)?;
        assert_eq!(budget_bytes(owner, OWNER_BUDGET)?, 132);
        sparse(&ws.join("builds/ff/twr"), OWNER_BUDGET)?;
        assert!(budget_bytes(owner, OWNER_BUDGET)? > OWNER_BUDGET);
        Ok(())
    }
    #[tokio::test]
    async fn build_capture_is_temporary_preserves_raw_bytes_and_accepts_source_archives(
    ) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("checkout");
        std::fs::create_dir(&root)?;
        let repo = Repository(root.clone());
        repo.git(&["init", "."]).await?;
        std::fs::write(root.join("file"), b"original\n")?;
        repo.git(&["add", "."]).await?;
        repo.git(&["commit", "-m", "initial"]).await?;
        std::fs::write(root.join(".gitignore"), "hidden\nprivate\n")?;
        std::fs::write(root.join("hidden"), b"tracked ignored addition\r\n")?;
        repo.git(&["add", "-f", "hidden"]).await?;
        std::fs::write(root.join("private"), "never export")?;
        std::fs::write(root.join("file"), b"raw bytes\r\n")?;
        std::fs::write(root.join(".gitattributes"), "* text filter=bad\n")?;
        repo.git(&["config", "filter.bad.clean", "false"]).await?;
        let index = std::fs::read(root.join(".git/index"))?;
        let objects = directory_bytes(&root.join(".git/objects"), u64::MAX)?;
        let bundle_path = dir.path().join("source.bundle");
        let bundle = repo.capture_bundle(&bundle_path).await?;
        assert_eq!(std::fs::read(root.join(".git/index"))?, index);
        assert_eq!(
            directory_bytes(&root.join(".git/objects"), u64::MAX)?,
            objects
        );
        assert!(repo
            .text(&["for-each-ref", "--format=%(refname)", "refs/twr/builds"])
            .await?
            .is_empty());
        let received = Repository::init(&dir.path().join("received")).await?;
        received
            .receive(&bundle_path, &bundle, "refs/twr/source")
            .await?;
        assert_eq!(
            received
                .git(&["show", &format!("{}:file", bundle.commit)])
                .await?,
            b"raw bytes\r\n"
        );
        assert_eq!(
            received
                .git(&["show", &format!("{}:hidden", bundle.commit)])
                .await?,
            b"tracked ignored addition\r\n"
        );
        assert!(!received
            .text(&["ls-tree", "--name-only", &bundle.commit])
            .await?
            .contains("private"));
        let archive = dir.path().join("archive");
        std::fs::create_dir(&archive)?;
        std::fs::write(archive.join("Cargo.toml"), "archive source")?;
        let bundle = Repository(archive.clone())
            .capture_bundle(&dir.path().join("archive.bundle"))
            .await?;
        assert!(valid_oid(&bundle.commit));
        assert!(!archive.join(".git").exists());
        Ok(())
    }
    #[tokio::test]
    async fn source_snapshot_and_bundle_round_trip_leave_checkout_and_index_unchanged() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("source");
        std::fs::create_dir(&root)?;
        let source = Repository(root.clone());
        source.git(&["init", "."]).await?;
        std::fs::write(root.join("file"), "first")?;
        source.git(&["add", "."]).await?;
        source.git(&["commit", "-m", "first"]).await?;
        let head = source.resolve("HEAD").await?;
        let index = std::fs::read(root.join(".git/index"))?;
        std::fs::write(root.join("file"), "edited")?;
        std::fs::write(root.join("untracked"), "included")?;
        std::fs::write(root.join(".gitignore"), "private\n")?;
        std::fs::write(root.join("private"), "secret")?;
        let commit = source.snapshot().await?;
        assert_eq!(source.resolve("HEAD").await?, head);
        assert_eq!(std::fs::read(root.join(".git/index"))?, index);
        let files = source.text(&["ls-tree", "--name-only", &commit]).await?;
        assert!(files.contains("untracked"));
        assert!(!files.contains("private"));
        let path = dir.path().join("source.bundle");
        let bundle = source
            .create_bundle(&commit, &path, &["ff".repeat(20)])
            .await?;
        let receiving = Repository::init(&dir.path().join("receiving")).await?;
        receiving
            .receive(&path, &bundle, "refs/twr/results/result")
            .await?;
        assert_eq!(receiving.resolve("refs/twr/results/result").await?, commit);
        assert!(receiving.resolve("HEAD").await.is_err());
        source
            .git(&["update-index", "--assume-unchanged", "file"])
            .await?;
        assert!(source.snapshot().await.is_err());
        Ok(())
    }
}

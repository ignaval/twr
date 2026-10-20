//! Core types for twr, a control tower for parallel coding agents.
//!
//! - [`event`]: the event envelope and every event kind
//! - [`activity`]: what a thread is doing, in a few words (its activity line's phrases)
//! - [`blob`]: content-addressed blob store trait and the 4 KB payload cap
//! - [`reduce`]: pure reducers (thread views, registry, attention stack, stack order)
//! - [`adapter`]: the runtime adapter trait and its channel-based handle
//! - [`git`]: the box git trait (worktrees, snapshots) and the porcelain v2 parser
//! - [`gitremote`]: git remote URLs: which twr clones, how they are shown and compared
//! - [`project_lookup`]: a git repository by its folder name under the project roots
//! - [`load`]: how loaded the machine is (pressure stall information, the load average)
//! - [`paths`], [`config`]: filesystem layout and `config.toml`
//! - [`notify`]: which control events deserve a user notification
//! - [`proto`], [`client`]: the hub socket protocol and an async client
//! - [`runner`]: the hub <-> runner WebSocket protocol
//! - [`sealed`]: the hub's box key and the sealed channel to an SSH box's daemon
//! - [`testkit`]: event-sequence builders for tests
//! - [`usage`]: subscription usage windows, their fold and how clients show them

pub mod activity;
pub mod adapter;
pub mod b64;
pub mod blob;
pub mod client;
pub mod config;
pub mod event;
pub mod git;
pub mod gitremote;
pub mod load;
pub mod notify;
pub mod paths;
pub mod project_lookup;
pub mod proto;
pub mod reduce;
pub mod runner;
pub mod sealed;
pub mod testkit;
pub mod usage;

/// twr's version: the package version, plus `+dev.<build>` for a build made from a checkout by
/// `docs/tools/dev-install.sh`, which sets `TWR_BUILD` (the commit, `.dirty` with uncommitted
/// changes) at compile time.
pub fn version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| match option_env!("TWR_BUILD") {
            Some(build) if !build.is_empty() => {
                format!("{}+dev.{build}", env!("CARGO_PKG_VERSION"))
            }
            _ => env!("CARGO_PKG_VERSION").to_string(),
        });
    &VERSION
}

/// Length of a build id ([`build_id`]) in hex digits.
pub const BUILD_ID_LEN: usize = 16;

/// The directory of build references in an SSH box's builds directory
/// (`<data>/twr/runner/builds/.refs/<hub id>-<box>`): one file per hub instance and box
/// name, listing the builds that hub uses there, one id per line, and one per build `twr box
/// install` put there for it (`<hub id>-<box>.install.<build id>`, until the hub runs it). The builds directory is
/// shared by every hub that reaches the box as that user; a hub pruning old builds keeps every
/// build any reference names (`twr_hub::ssh_box::prune_script`), and the relay adds its own
/// build to its hub's reference as it starts (`twr_runner::relay`).
pub const BUILD_REFS_DIR: &str = ".refs";

/// The lock of [`BUILD_REFS_DIR`], next to it in the builds directory: a directory made with
/// `mkdir` (atomic on every system, which `flock(1)` is not: macOS has none), removed when the
/// writer is done; one older than [`BUILD_REFS_LOCK_STALE_SECS`] was left by a writer that
/// died and is removed. Held by a hub's prune for its whole read, write and removal, and by
/// the relay while it adds its build to its hub's reference. `twr box install` takes none: it
/// only adds a file of its own (`<hub id>-<box>.install.<build id>`), atomically.
pub const BUILD_REFS_LOCK: &str = ".refs.lock";

/// How old a [`BUILD_REFS_LOCK`] must be to count as left by a writer that died.
pub const BUILD_REFS_LOCK_STALE_SECS: u64 = 120;

/// How long a writer waits for [`BUILD_REFS_LOCK`] before it gives up (the prune: it prunes
/// nothing; the relay: it records nothing).
pub const BUILD_REFS_LOCK_WAIT_SECS: u64 = 20;

/// The running executable's build id ([`build_id_of`] of it), computed on the first call (it
/// reads the whole file) and kept: on Linux of `/proc/self/exe`, which is the running build
/// even after its file was replaced or deleted (an upgrade), elsewhere of
/// [`std::env::current_exe`]. `None` when it cannot be read.
///
/// It names a build where the version cannot: every development build of one commit with
/// uncommitted changes has the same version string. The hub installs builds on SSH boxes
/// under it and checks that a box runs the one it launched.
pub fn build_id() -> Option<&'static str> {
    static ID: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| {
        let exe = if cfg!(target_os = "linux") {
            std::path::PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe().ok()?
        };
        build_id_of(&exe).ok()
    });
    ID.as_deref()
}

/// The executable to start another twr process with (the hub that `twr` and the TUI start):
/// [`std::env::current_exe`], except after the file this process runs was replaced. An
/// upgrade (`twr update`, the installer, `docs/tools/dev-install.sh`) renames the new build
/// over the old one, and on Linux `current_exe` then reads `<path> (deleted)`, a path with no
/// file, so starting it failed with "No such file or directory": the build now at `<path>` is
/// started instead, as `twr` run again would. When no file is left at the path (twr was
/// removed or moved), the error says so and what to do.
pub fn current_exe() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("locating the twr executable")?;
    exe_to_start(exe, |p| p.is_file())
}

/// [`current_exe`] of what `std::env::current_exe` read (`exe`), with `is_file` telling which
/// paths name a file.
fn exe_to_start(
    exe: std::path::PathBuf,
    is_file: impl Fn(&std::path::Path) -> bool,
) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    if is_file(&exe) {
        return Ok(exe);
    }
    // What the kernel appends to `/proc/self/exe` once the file is gone from its path.
    let replaced = exe
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
        .map(|p| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(p)));
    match replaced {
        Some(path) if is_file(&path) => Ok(path),
        replaced => Err(anyhow::anyhow!(
            "the twr executable this process started from, {}, is gone (removed or moved since \
             it started): start twr again from a shell where the installed twr is on PATH \
             (quit and reopen the TUI)",
            replaced.as_deref().unwrap_or(&exe).display()
        )),
    }
}

/// The build id of the executable at `path`: the first [`BUILD_ID_LEN`] lowercase hex digits
/// of the SHA-256 of the file's contents.
pub fn build_id_of(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 256 << 10];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => ctx.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hex::encode(&ctx.finish().as_ref()[..BUILD_ID_LEN / 2]))
}

/// Whether `id` has the shape of a build id ([`build_id_of`]): exactly [`BUILD_ID_LEN`]
/// lowercase hex digits.
pub fn valid_build_id(id: &str) -> bool {
    id.len() == BUILD_ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub use event::{Event, EventBody, Source, HUB_THREAD};

#[cfg(test)]
mod tests {
    /// A build id is the head of the file's SHA-256, and the running test binary has one.
    #[test]
    fn build_ids() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("twr");
        std::fs::write(&f, b"abc").unwrap();
        // SHA-256("abc") = ba7816bf8f01cfea...
        assert_eq!(super::build_id_of(&f).unwrap(), "ba7816bf8f01cfea");
        assert!(super::build_id_of(&dir.path().join("missing")).is_err());
        let me = super::build_id().unwrap();
        assert!(super::valid_build_id(me), "{me}");
        assert_eq!(super::build_id(), Some(me), "computed once");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(super::build_id_of(&exe).unwrap(), me);
        for bad in [
            "",
            "ba7816bf8f01cfe",
            "BA7816BF8F01CFEA",
            "ba7816bf8f01cfeg",
            "ba7816bf8f01cfea0",
        ] {
            assert!(!super::valid_build_id(bad), "{bad}");
        }
    }

    /// The path `current_exe` read is started when it names a file; `<path> (deleted)` (Linux,
    /// once the file was replaced) starts what replaced it, and fails when nothing did.
    #[test]
    fn the_exe_to_start_survives_a_replaced_file() {
        use std::path::{Path, PathBuf};
        let files = [Path::new("/bin/twr"), Path::new("/odd/twr (deleted)")];
        let is_file = |p: &Path| files.contains(&p);
        let start = |p: &str| super::exe_to_start(PathBuf::from(p), is_file);
        assert_eq!(start("/bin/twr").unwrap(), Path::new("/bin/twr"));
        assert_eq!(start("/bin/twr (deleted)").unwrap(), Path::new("/bin/twr"));
        // A file really named so.
        assert_eq!(
            start("/odd/twr (deleted)").unwrap(),
            Path::new("/odd/twr (deleted)")
        );
        for gone in ["/gone/twr (deleted)", "/gone/twr"] {
            let e = start(gone).unwrap_err().to_string();
            assert!(e.contains("/gone/twr, is gone"), "{e}");
            assert!(e.contains("start twr again"), "{e}");
        }
    }

    /// What [`replaced_exe_child`] runs as: the gate file it waits for.
    const EXE_GATE_ENV: &str = "TWR_TEST_EXE_GATE";

    /// Run by [`a_running_process_starts_the_build_that_replaced_it`] from a copy of this test
    /// binary (nothing otherwise): once the gate file exists, prints what
    /// [`super::current_exe`] says.
    #[test]
    fn replaced_exe_child() {
        let Some(gate) = std::env::var_os(EXE_GATE_ENV) else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !std::path::Path::new(&gate).exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match super::current_exe() {
            Ok(p) => println!("exe=ok:{}", p.display()),
            Err(e) => println!("exe=err:{e:#}"),
        }
    }

    /// The real case: a process whose executable is renamed over (an upgrade) while it runs
    /// starts the new build, and one whose executable is removed says so. Before, on Linux,
    /// it tried `<path> (deleted)`: "No such file or directory".
    #[test]
    fn a_running_process_starts_the_build_that_replaced_it() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::env::current_exe().unwrap();
        let run = |replace: bool| {
            let exe = dir.path().join(if replace { "twr-up" } else { "twr-rm" });
            let gate = dir.path().join(format!("gate-{replace}"));
            std::fs::copy(&me, &exe).unwrap();
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(["--exact", "tests::replaced_exe_child", "--nocapture", "-q"])
                .env(EXE_GATE_ENV, &gate)
                .stdout(std::process::Stdio::piped());
            // A file just written can be busy for a moment ("Text file busy"): another test's
            // fork may hold it open until its exec.
            let mut tries = 0;
            let child = loop {
                match cmd.spawn() {
                    Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    r => break r.unwrap(),
                }
            };
            if replace {
                let new = dir.path().join("twr-new");
                std::fs::copy(&me, &new).unwrap();
                std::fs::rename(&new, &exe).unwrap();
            } else {
                std::fs::remove_file(&exe).unwrap();
            }
            std::fs::write(&gate, "").unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let out = String::from_utf8(out.stdout).unwrap();
            let line = out.lines().find(|l| l.starts_with("exe=")).unwrap_or("");
            (exe, line.to_string())
        };
        let (exe, line) = run(true);
        assert_eq!(line, format!("exe=ok:{}", exe.display()));
        let (exe, line) = run(false);
        assert!(
            line.starts_with(&format!("exe=err:the twr executable this process started from, {}, is gone", exe.display())),
            "{line}"
        );
    }
}

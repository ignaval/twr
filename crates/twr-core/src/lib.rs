//! Core types for twr, a control tower for parallel coding agents.
//!
//! - [`event`]: the event envelope and every event kind
//! - [`activity`]: what a thread is doing, in a few words (its activity line's phrases)
//! - [`blob`]: content-addressed blob store trait and the 4 KB payload cap
//! - [`reduce`]: pure reducers (thread views, registry, attention stack, stack order)
//! - [`adapter`]: the runtime adapter trait and its channel-based handle
//! - [`git`]: the box git trait (worktrees, snapshots) and the porcelain v2 parser
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
pub mod load;
pub mod notify;
pub mod paths;
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
}

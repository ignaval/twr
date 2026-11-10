//! Noticing that the directory a long-lived process serves is gone, so that it ends instead of
//! running for ever: the linked-runner service and its managed workers watch the twr home they
//! were started for (`twr-link`'s service, [`crate::managed::revoked`]).
//!
//! A home deleted without `twr runner stop` first (a test's temporary home, a workspace's data
//! removed by hand) leaves a service whose socket lives elsewhere (the per-user temporary
//! directory) and whose launchd job is never booted out: it would run, and its workers with
//! it, an executable whose file is gone, for a root that is gone.
//!
//! What counts ([`look`]): the directory recorded at start ([`Anchor`]) is
//!
//! - [`Seen::There`]: still at its path, the same directory (device and inode);
//! - [`Seen::Gone`]: not at its path while the filesystem it was on still is: the nearest
//!   ancestor that is there is on the same device (deleted, or moved away), or its path names
//!   another directory of that device (deleted and made again);
//! - [`Seen::Unknown`]: anything else, which never counts: an error other than "no such file"
//!   (permission, I/O, a stale network handle), a check that did not finish in time
//!   ([`LOOK_TIMEOUT`]), or a nearest ancestor on another device (the volume it was on is not
//!   mounted: an external disk, a network home after sleep).
//!
//! [`GoneWatch`] gives up only once it was [`Seen::Gone`] at [`GONE_SEEN`] looks in a row
//! spanning [`GONE_FOR`] at least: anything else between them starts over. Its clock
//! ([`std::time::Instant`]) does not run while the machine sleeps on macOS, and the looks are
//! counted too, so a wake does not make a long absence out of a short one.
//!
//! Then the process ends as it does when asked to, and, when launchd runs it as one of twr's
//! jobs, takes its job away ([`unregister_own_job`]); a `systemd-run` unit of twr's is
//! collected once its process ends (`--collect`), and a process started on its own just ends.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the directory must have been gone, at least, before a process gives up on it.
pub const GONE_FOR: Duration = Duration::from_secs(5 * 60);

/// How many looks in a row must have found it gone, at least.
pub const GONE_SEEN: u32 = 5;

/// How often a process looks.
pub const LOOK_EVERY: Duration = Duration::from_secs(30);

/// How long one look may take (a hung network filesystem): longer is [`Seen::Unknown`].
pub const LOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// A directory as it was when the process started: its path, device and inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl Anchor {
    /// `path` as it is now: an error when it is not a directory that can be read.
    pub fn of(path: &Path) -> std::io::Result<Anchor> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?;
        if !m.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ));
        }
        Ok(Anchor {
            path: path.to_path_buf(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }
}

/// What a look at an [`Anchor`] found (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    There,
    Gone,
    Unknown,
}

/// Looks at `anchor` now (it may block on a slow filesystem: [`look_bounded`] does not).
pub fn look(anchor: &Anchor) -> Seen {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(&anchor.path) {
        Ok(m) if m.is_dir() && m.dev() == anchor.dev && m.ino() == anchor.ino => Seen::There,
        // Another directory (or file) of the same filesystem where it was: it was deleted or
        // moved away, and something else made there.
        Ok(m) if m.dev() == anchor.dev => Seen::Gone,
        // Another filesystem there (mounted over it): nothing is known of the directory.
        Ok(_) => Seen::Unknown,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            match nearest_device(&anchor.path) {
                Some(dev) if dev == anchor.dev => Seen::Gone,
                _ => Seen::Unknown,
            }
        }
        Err(_) => Seen::Unknown,
    }
}

/// The device of the nearest ancestor of `path` that is there (`None` when none can be read).
fn nearest_device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut at = path.parent();
    while let Some(dir) = at {
        match std::fs::metadata(dir) {
            Ok(m) => return Some(m.dev()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                at = dir.parent();
            }
            Err(_) => return None,
        }
    }
    None
}

/// [`look`] on a blocking thread, at most [`LOOK_TIMEOUT`]: [`Seen::Unknown`] when it takes
/// longer (the thread finishes on its own).
pub async fn look_bounded(anchor: &Anchor) -> Seen {
    let anchor = anchor.clone();
    match tokio::time::timeout(
        LOOK_TIMEOUT,
        tokio::task::spawn_blocking(move || look(&anchor)),
    )
    .await
    {
        Ok(Ok(seen)) => seen,
        _ => Seen::Unknown,
    }
}

/// Whether a directory has been gone long enough to give up on it (see the module docs).
#[derive(Debug, Default, Clone)]
pub struct GoneWatch {
    since: Option<Instant>,
    seen: u32,
}

impl GoneWatch {
    /// Takes the look `seen` made at `now`: true once the directory was [`Seen::Gone`] at
    /// [`GONE_SEEN`] looks in a row spanning [`GONE_FOR`]; any other look starts over.
    pub fn observe(&mut self, seen: Seen, now: Instant) -> bool {
        if seen != Seen::Gone {
            *self = GoneWatch::default();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        self.seen = self.seen.saturating_add(1);
        self.seen >= GONE_SEEN && now.saturating_duration_since(since) >= GONE_FOR
    }
}

/// Watches `anchor` every [`LOOK_EVERY`] until it has been gone long enough ([`GoneWatch`]):
/// returns then, never otherwise.
pub async fn until_gone(anchor: Anchor) {
    let mut watch = GoneWatch::default();
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let seen = look_bounded(&anchor).await;
        if seen == Seen::Gone {
            tracing::warn!(path = %anchor.path.display(), "the directory this process serves is gone");
        }
        if watch.observe(seen, Instant::now()) {
            return;
        }
    }
}

/// The label of this process's launchd job when launchd runs it as one of twr's
/// (`XPC_SERVICE_NAME` is `tools.twr.box.<...>`, [`crate::relay::launchd::LABEL_PREFIX`]).
pub fn own_launchd_label() -> Option<String> {
    label_of(&std::env::var("XPC_SERVICE_NAME").ok()?)
}

/// `label` when it is one of twr's launchd jobs' (letters, digits and dots after the prefix).
fn label_of(label: &str) -> Option<String> {
    let rest = label.strip_prefix(crate::relay::launchd::LABEL_PREFIX)?;
    (!rest.is_empty()
        && label.len() <= 128
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
    .then(|| label.to_string())
}

/// Takes this process's own launchd job away (`launchctl bootout gui/<uid>/<label>`), when
/// launchd runs it as one of twr's ([`own_launchd_label`]), so that it does not stay loaded
/// once this process ends: the last thing a process does before it exits. `launchctl` runs in
/// a session of its own (it outlives this process's group), and is waited for at most a few
/// seconds: launchd sends this process SIGTERM as it takes the job away, and `launchctl` waits
/// for it to end. Nothing on another OS, or for a process launchd does not run.
pub fn unregister_own_job() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some(label) = own_launchd_label() else {
        return;
    };
    bootout(
        &["/bin/launchctl".into()],
        &format!("gui/{}", twr_core::paths::effective_uid()),
        &label,
        Duration::from_secs(5),
    );
}

/// Runs `launchctl bootout <domain>/<label>` with `launchctl` (`/bin/launchctl`, or a test's
/// `sh <script>`) in a session of its own, waiting at most `wait` for it
/// ([`unregister_own_job`]). Whether it could be run.
fn bootout(launchctl: &[std::ffi::OsString], domain: &str, label: &str, wait: Duration) -> bool {
    let Some((program, lead)) = launchctl.split_first() else {
        return false;
    };
    let target = format!("{domain}/{label}");
    tracing::info!(job = %target, "taking this process's launchd job away");
    let mut cmd = std::process::Command::new(program);
    cmd.args(lead)
        .args(["bootout", &target])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    twr_adapters::process::new_session_std(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        tracing::warn!(job = %target, "could not run launchctl to take the job away");
        return false;
    };
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Still waiting for this process to end, as launchctl does: it goes on unwaited for.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gone only at [`GONE_SEEN`] gone looks in a row spanning [`GONE_FOR`]: fewer looks, a
    /// shorter span, or one look that found it there or could not tell, and it starts over.
    #[test]
    fn a_watch_gives_up_only_on_a_long_unbroken_absence() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = GoneWatch::default();
        for i in 0..10 {
            assert!(!w.observe(Seen::Gone, at(i * 30)), "look {i}");
        }
        assert!(w.observe(Seen::Gone, at(300)));
        // Many looks, too short a time (a wake: looks bunched up).
        let mut w = GoneWatch::default();
        for i in 0..20 {
            assert!(!w.observe(Seen::Gone, at(i)), "look {i}");
        }
        // A long time, too few looks.
        let mut w = GoneWatch::default();
        assert!(!w.observe(Seen::Gone, at(0)));
        assert!(!w.observe(Seen::Gone, at(3600)));
        assert!(!w.observe(Seen::Gone, at(7200)));
        // Anything else between starts over.
        for other in [Seen::There, Seen::Unknown] {
            let mut w = GoneWatch::default();
            for i in 0..9 {
                w.observe(Seen::Gone, at(i * 30));
            }
            assert!(!w.observe(other, at(270)));
            for i in 10..19 {
                assert!(!w.observe(Seen::Gone, at(i * 30)), "{other:?} look {i}");
            }
            assert!(w.observe(Seen::Gone, at(19 * 30 + 300)));
        }
    }

    /// A look: there while it is; gone once deleted (its parent on the same filesystem), or
    /// deleted with its parents, or made again (another inode), or replaced by a file;
    /// unknown when it can't be read for another reason.
    #[test]
    fn a_look_tells_gone_from_unknown() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home/data");
        std::fs::create_dir_all(&home).unwrap();
        let anchor = Anchor::of(&home).unwrap();
        assert_eq!(look(&anchor), Seen::There);
        std::fs::remove_dir(&home).unwrap();
        assert_eq!(look(&anchor), Seen::Gone);
        std::fs::remove_dir(d.path().join("home")).unwrap();
        assert_eq!(look(&anchor), Seen::Gone);
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(look(&anchor), Seen::Gone, "made again: another directory");
        std::fs::remove_dir(&home).unwrap();
        std::fs::write(&home, "x").unwrap();
        assert_eq!(look(&anchor), Seen::Gone, "a file where it was");
        std::fs::remove_file(&home).unwrap();
        // Its device is no ancestor's: the volume it was on is not there.
        let elsewhere = Anchor {
            dev: anchor.dev.wrapping_add(1),
            ..anchor.clone()
        };
        assert_eq!(look(&elsewhere), Seen::Unknown);
        // Moved away: gone from its path.
        std::fs::create_dir_all(&home).unwrap();
        let moved = Anchor::of(&home).unwrap();
        std::fs::rename(&home, d.path().join("moved")).unwrap();
        assert_eq!(look(&moved), Seen::Gone);
        // A parent this user can't search: not known (root can search it anyway).
        if twr_core::paths::effective_uid() != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = d.path().join("locked");
            std::fs::create_dir_all(locked.join("data")).unwrap();
            let anchor = Anchor::of(&locked.join("data")).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let seen = look(&anchor);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(seen, Seen::Unknown);
        }
        assert!(Anchor::of(&d.path().join("nothing")).is_err());
        std::fs::write(d.path().join("file"), "").unwrap();
        assert!(Anchor::of(&d.path().join("file")).is_err());
    }

    /// The job taken away is exactly the label given, in the domain given, by `launchctl
    /// bootout` (a fake one here); one that waits for this process to end, as the real one
    /// does, is not waited for long.
    #[test]
    fn bootout_takes_away_exactly_its_own_label() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("launchctl.sh");
        std::fs::write(
            &script,
            "d=$(dirname \"$0\")\necho \"$*\" >> \"$d/calls\"\n[ -f \"$d/hang\" ] && sleep 30\nexit 0\n",
        )
        .unwrap();
        let fake = ["/bin/sh".into(), script.clone().into_os_string()];
        let label = "tools.twr.box.31c5e3e05b975a7f.27a95394";
        assert!(bootout(&fake, "gui/501", label, Duration::from_secs(5)));
        assert_eq!(
            std::fs::read_to_string(d.path().join("calls")).unwrap(),
            format!("bootout gui/501/{label}\n")
        );
        std::fs::write(d.path().join("hang"), "").unwrap();
        let t = Instant::now();
        assert!(bootout(&fake, "gui/501", label, Duration::from_millis(300)));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(!bootout(
            &[d.path().join("no-launchctl").into_os_string()],
            "gui/501",
            label,
            Duration::from_secs(1)
        ));
    }

    /// Only a label of twr's own jobs is taken for this process's.
    #[test]
    fn only_twrs_own_launchd_labels_are_taken_away() {
        let check = |v: &str| label_of(v).is_some();
        assert!(check("tools.twr.box.31c5e3e05b975a7f.27a95394"));
        assert!(!check("com.apple.Terminal"));
        assert!(!check("tools.twr.box."));
        assert!(!check("tools.twr.box.a b"));
        assert!(!check("tools.twr.box.a/b"));
    }
}

//! What a speaker keeps on its device: `speech-state.json` in the root's data directory.
//!
//! - `device`: the speaker's id, random, made once (a hub keeps one speaker per device).
//! - `live`: live reading per `<host>/<workspace>` (`local` for this machine's hubs, else the
//!   `--ssh` host as given). Off unless turned on (`/read on`).
//! - `watermarks`: per thread id, the last seq read (spoken or skipped): a reconnect resumes
//!   after it. At most [`WATERMARKS_MAX`] threads, the most recently updated.
//! - `voice`, `rate`: set by `/read voice` and `/read rate`.
//!
//! Each change is read-modify-write under a lock file ([`StateStore::update`]), so two
//! speakers of one device (a TUI's and `twr speak`) never lose each other's changes; the file
//! is written whole (a new file renamed over it), mode 0600. A file that can't be read starts
//! over (with a warning and a new device id).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub use twr_core::speech::WATERMARKS_MAX;

/// The state file's name, in the root's data directory.
pub const STATE_FILE: &str = "speech-state.json";

/// The host part of a live switch's key for this machine's hubs.
pub const LOCAL_HOST: &str = "local";

/// A speaker's state (see the module documentation).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeechState {
    /// The device id: 16 hex digits.
    pub device: String,
    /// The speaker's name, if one was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Live reading per `<host>/<workspace>` ([`live_key`]).
    #[serde(default)]
    pub live: BTreeMap<String, bool>,
    /// The last seq read per thread id.
    #[serde(default)]
    pub watermarks: BTreeMap<String, u64>,
    /// When each watermark was last updated, as a counter (the cap keeps the latest).
    #[serde(default)]
    pub touched: BTreeMap<String, u64>,
    /// The voice `/read voice` set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The rate `/read rate` set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
}

/// The key of a live switch: `<host>/<ws>`.
pub fn live_key(host: &str, ws: &str) -> String {
    format!("{host}/{ws}")
}

/// A new device id: 16 random hex digits.
pub fn new_device_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Whether `id` is a device id a hub takes (1-64 of `[A-Za-z0-9_-]`).
fn device_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl SpeechState {
    /// A fresh state with a new device id.
    pub fn fresh() -> SpeechState {
        SpeechState {
            device: new_device_id(),
            ..Default::default()
        }
    }

    /// Whether live reading is on for `<host>/<ws>`.
    pub fn live(&self, host: &str, ws: &str) -> bool {
        self.live.get(&live_key(host, ws)).copied().unwrap_or(false)
    }

    /// `thread`'s watermark.
    pub fn watermark(&self, thread: &str) -> Option<u64> {
        self.watermarks.get(thread).copied()
    }

    /// Moves `thread`'s watermark up to `seq` (never down), and keeps the latest
    /// [`WATERMARKS_MAX`].
    pub fn set_watermark(&mut self, thread: &str, seq: u64) {
        let w = self.watermarks.entry(thread.to_string()).or_insert(seq);
        *w = (*w).max(seq);
        self.touch(thread);
    }

    /// Gives `thread` the watermark `seq` unless it has one.
    pub fn init_watermark(&mut self, thread: &str, seq: u64) {
        if !self.watermarks.contains_key(thread) {
            self.watermarks.insert(thread.to_string(), seq);
            self.touch(thread);
        }
    }

    /// Marks `thread`'s watermark as just updated, and drops the oldest beyond the cap.
    fn touch(&mut self, thread: &str) {
        let next = self.touched.values().max().map_or(1, |m| m + 1);
        self.touched.insert(thread.to_string(), next);
        self.cap();
    }

    /// Keeps the [`WATERMARKS_MAX`] most recently updated watermarks (one never touched counts
    /// as oldest), and only `touched` entries of watermarks there are.
    fn cap(&mut self) {
        let watermarks = &self.watermarks;
        self.touched.retain(|t, _| watermarks.contains_key(t));
        if self.watermarks.len() <= WATERMARKS_MAX {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .watermarks
            .keys()
            .map(|t| (self.touched.get(t).copied().unwrap_or(0), t.clone()))
            .collect();
        by_age.sort();
        let extra = self.watermarks.len() - WATERMARKS_MAX;
        for (_, t) in by_age.into_iter().take(extra) {
            self.watermarks.remove(&t);
            self.touched.remove(&t);
        }
    }
}

/// The state file and its lock.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// The state of a root whose data directory is `data_dir` ([`STATE_FILE`] in it).
    pub fn in_data_dir(data_dir: &Path) -> StateStore {
        StateStore::at(data_dir.join(STATE_FILE))
    }

    /// The state file at `path`.
    pub fn at(path: PathBuf) -> StateStore {
        StateStore { path }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The state as it is (made, with a device id, the first time).
    pub fn load(&self) -> anyhow::Result<SpeechState> {
        self.update(|_| {})
    }

    /// Applies `change` to the state as it is in the file, under the lock, and writes it back;
    /// the state written.
    pub fn update(&self, change: impl FnOnce(&mut SpeechState)) -> anyhow::Result<SpeechState> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        twr_core::paths::ensure_private_dir(dir)?;
        let _lock = Lock::take(&self.lock_path())?;
        let before = self.read();
        let mut state = before.clone().unwrap_or_else(SpeechState::fresh);
        change(&mut state);
        state.cap();
        if before.as_ref() != Some(&state) {
            self.write(&state)?;
        }
        Ok(state)
    }

    /// The lock file.
    fn lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        self.path.with_file_name(name)
    }

    /// The state in the file; `None` when there is none, or it can't be read (a warning).
    fn read(&self) -> Option<SpeechState> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("reading {}: {e}; starting over", self.path.display());
                return None;
            }
        };
        match serde_json::from_str::<SpeechState>(&text) {
            Ok(s) if device_ok(&s.device) => Some(s),
            Ok(_) => {
                tracing::warn!(
                    "{}: no usable device id; starting over",
                    self.path.display()
                );
                None
            }
            Err(e) => {
                tracing::warn!("{} is unreadable ({e}); starting over", self.path.display());
                None
            }
        }
    }

    /// Writes `state` whole: a new file (0600) renamed over the old one.
    fn write(&self, state: &SpeechState) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".tmp.{}", std::process::id()));
        let tmp = self.path.with_file_name(name);
        let text = serde_json::to_string_pretty(state)?;
        let result = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            twr_core::paths::restrict_file(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.with_context(|| format!("writing {}", self.path.display()))
    }
}

/// The speaking of one machine's hubs, held by one speaker of this device at a time
/// ([`StateStore::try_own`]): an exclusive `flock` on `speech-state.json.owner-<host>`,
/// released when dropped or when the process ends, however it ends.
#[derive(Debug)]
pub struct Ownership(#[allow(dead_code)] std::fs::File);

impl StateStore {
    /// Takes the speaking of `host`'s hubs (`local`, or an ssh host) if no other speaker of
    /// this device holds it: `None` while one does (this one then stands by). Two speakers of
    /// one device for the same hubs (the TUI's own and `twr speak`, two `twr speak --ssh` of one
    /// server) thus never both play, whichever of those hubs each could reach; a speaker for
    /// another machine's hubs is not held up.
    pub fn try_own(&self, host: &str) -> anyhow::Result<Option<Ownership>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            twr_core::paths::ensure_private_dir(dir)?;
        }
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".owner-{}", hex_name(host)));
        let path = self.path.with_file_name(name);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        loop {
            // SAFETY: `flock` on a descriptor this function owns, open for its whole call.
            let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if r == 0 {
                return Ok(Some(Ownership(f)));
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return Ok(None),
                _ => {
                    return Err(anyhow::Error::new(e).context(format!("locking {}", path.display())))
                }
            }
        }
    }
}

/// `host` as a file name: its bytes in hex (any host, any characters, one name each).
fn hex_name(host: &str) -> String {
    host.bytes().map(|b| format!("{b:02x}")).collect()
}

/// An exclusive `flock` on the lock file, released when dropped (the file closed).
struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    fn take(path: &Path) -> anyhow::Result<Lock> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        loop {
            // SAFETY: `flock` on a descriptor this function owns, open for its whole call.
            let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            if r == 0 {
                return Ok(Lock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(anyhow::Error::new(e).context(format!("locking {}", path.display())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// One speaker of a machine speaks: a second one's `try_own` waits for the first to let go.
    #[test]
    fn one_speaker_owns_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let a = StateStore::in_data_dir(dir.path());
        let b = StateStore::in_data_dir(dir.path());
        let held = a.try_own("local").unwrap().expect("the first takes it");
        assert!(
            b.try_own("local").unwrap().is_none(),
            "the second stands by"
        );
        let other = b.try_own("me@server").unwrap();
        assert!(other.is_some(), "another machine's hubs are not held up");
        drop(held);
        assert!(b.try_own("local").unwrap().is_some(), "then takes it");
    }

    #[test]
    fn the_state_survives_a_reload_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::in_data_dir(&dir.path().join("data"));
        let first = store.load().unwrap();
        assert_eq!(first.device.len(), 16);
        assert!(first.device.chars().all(|c| c.is_ascii_hexdigit()));
        store
            .update(|s| {
                s.set_watermark("T1", 7);
                s.set_watermark("T1", 3);
                s.init_watermark("T1", 1);
                s.init_watermark("T2", 0);
                s.live.insert(live_key("local", "default"), true);
                s.voice = Some("Ava".into());
            })
            .unwrap();
        let again = StateStore::in_data_dir(&dir.path().join("data"))
            .load()
            .unwrap();
        assert_eq!(again.device, first.device);
        assert_eq!(again.watermark("T1"), Some(7));
        assert_eq!(again.watermark("T2"), Some(0));
        assert!(again.live("local", "default"));
        assert!(!again.live("local", "other"));
        assert_eq!(again.voice.as_deref(), Some("Ava"));
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dmode = std::fs::metadata(dir.path().join("data"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dmode & 0o777, 0o700);
    }

    #[test]
    fn a_corrupt_file_starts_over_with_a_new_device() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::in_data_dir(dir.path());
        let first = store.load().unwrap();
        std::fs::write(store.path(), "{not json").unwrap();
        let next = store.load().unwrap();
        assert_ne!(next.device, first.device);
        assert!(next.watermarks.is_empty());
    }

    #[test]
    fn watermarks_keep_the_most_recently_updated() {
        let mut s = SpeechState::fresh();
        for i in 0..WATERMARKS_MAX + 10 {
            s.set_watermark(&format!("T{i:04}"), 1);
        }
        // The first ever is updated again: it stays.
        s.set_watermark("T0000", 2);
        s.set_watermark("NEW", 1);
        assert_eq!(s.watermarks.len(), WATERMARKS_MAX);
        assert_eq!(s.touched.len(), WATERMARKS_MAX);
        assert_eq!(s.watermark("T0000"), Some(2));
        assert_eq!(s.watermark("NEW"), Some(1));
        assert_eq!(s.watermark("T0001"), None);
    }

    #[test]
    fn two_stores_of_one_file_keep_each_others_changes() {
        let dir = tempfile::tempdir().unwrap();
        let a = StateStore::in_data_dir(dir.path());
        let b = StateStore::in_data_dir(dir.path());
        a.update(|s| s.set_watermark("A", 1)).unwrap();
        b.update(|s| s.set_watermark("B", 2)).unwrap();
        let s = a.update(|s| s.set_watermark("A", 3)).unwrap();
        assert_eq!((s.watermark("A"), s.watermark("B")), (Some(3), Some(2)));
    }
}

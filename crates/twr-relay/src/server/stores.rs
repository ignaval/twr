//! The relay's small stores: pkarr records (memory only), iroh admissions, moved notes and
//! per-hub daily byte totals (in the state directory). docs/relay-protocol.md §5.7, §9, §10.

use super::certs::write_private;
use crate::{b32, keys};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---- Records --------------------------------------------------------------------------------

pub const MAX_PACKET: usize = 1000;

#[derive(Debug, Clone)]
pub struct Record {
    /// The relay payload: signature, timestamp, packet.
    pub payload: Bytes,
    pub timestamp_us: u64,
    pub stored_unix: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PutError {
    Bad,
    Older,
    SameTimestamp,
    Full,
}

/// The BEP 44 signable of a pkarr packet.
pub fn signable(timestamp_us: u64, packet: &[u8]) -> Vec<u8> {
    let mut s = format!("3:seqi{}e1:v{}:", timestamp_us, packet.len()).into_bytes();
    s.extend_from_slice(packet);
    s
}

pub fn parse_pkarr_key(z32: &str) -> Option<[u8; 32]> {
    if z32.len() != 52 {
        return None;
    }
    b32::zdecode(z32)?.try_into().ok()
}

#[derive(Debug, Default)]
pub struct Records {
    map: Mutex<HashMap<[u8; 32], Record>>,
}

impl Records {
    /// Checks and stores a PUT body (§9).
    pub fn put(&self, key: &[u8; 32], body: &[u8], now: u64, max: usize) -> Result<(), PutError> {
        if body.len() < 72 || body.len() > 72 + MAX_PACKET {
            return Err(PutError::Bad);
        }
        let (sig, rest) = body.split_at(64);
        let ts = u64::from_be_bytes(rest[..8].try_into().unwrap());
        let packet = &rest[8..];
        if !keys::verify_ed25519(key, &signable(ts, packet), sig) {
            return Err(PutError::Bad);
        }
        let mut map = self.map.lock().unwrap();
        if let Some(old) = map.get(key) {
            if old.timestamp_us > ts {
                return Err(PutError::Older);
            }
            if old.timestamp_us == ts && old.payload.as_ref() != body {
                return Err(PutError::SameTimestamp);
            }
        } else if map.len() >= max {
            return Err(PutError::Full);
        }
        map.insert(
            *key,
            Record {
                payload: Bytes::copy_from_slice(body),
                timestamp_us: ts,
                stored_unix: now,
            },
        );
        Ok(())
    }

    pub fn get(&self, key: &[u8; 32], now: u64, ttl: u64) -> Option<Record> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|r| r.stored_unix + ttl > now).cloned()
    }

    /// Drops expired records; returns how many remain.
    pub fn sweep(&self, now: u64, ttl: u64) -> usize {
        let mut map = self.map.lock().unwrap();
        map.retain(|_, r| r.stored_unix + ttl > now);
        map.len()
    }
}

/// The admission table is full.
#[derive(Debug, PartialEq, Eq)]
pub struct Full;

// ---- Persisted state --------------------------------------------------------------------------

/// Admissions, moved notes and usage, saved as one JSON file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    admissions: BTreeMap<String, u64>,
    #[serde(default)]
    moved: BTreeMap<String, MovedNote>,
    /// hub name → (day number → bytes).
    #[serde(default)]
    usage: BTreeMap<String, BTreeMap<u64, u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MovedNote {
    pub spki: String,
    pub body: String,
    pub sig: String,
    pub stored: u64,
}

pub const MOVED_DAYS: u64 = 90;
pub const USAGE_DAYS: u64 = 30;
pub const STATE_FILE: &str = "state.json";

pub struct State {
    path: PathBuf,
    admissions: Mutex<HashMap<[u8; 32], u64>>,
    moved: Mutex<HashMap<String, MovedNote>>,
    usage: Mutex<HashMap<String, BTreeMap<u64, u64>>>,
    dirty: std::sync::atomic::AtomicBool,
}

impl State {
    pub fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let path = state_dir.join(STATE_FILE);
        let saved: Saved = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(
                    "{} is unreadable ({e}); starting with empty state",
                    path.display()
                );
                Saved::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e.into()),
        };
        let admissions = saved
            .admissions
            .into_iter()
            .filter_map(|(k, v)| Some((crate::admit::parse_endpoint(&k)?, v)))
            .collect();
        Ok(Self {
            path,
            admissions: Mutex::new(admissions),
            moved: Mutex::new(saved.moved.into_iter().collect()),
            usage: Mutex::new(saved.usage.into_iter().collect()),
            dirty: false.into(),
        })
    }

    fn mark(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Saves if anything changed since the last save.
    pub fn save(&self) -> anyhow::Result<()> {
        if !self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let saved = Saved {
            admissions: self
                .admissions
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (hex::encode(k), *v))
                .collect(),
            moved: self.moved.lock().unwrap().clone().into_iter().collect(),
            usage: self.usage.lock().unwrap().clone().into_iter().collect(),
        };
        if let Err(e) = write_private(&self.path, &serde_json::to_vec(&saved)?) {
            self.mark();
            return Err(e);
        }
        Ok(())
    }

    /// Drops what has expired. Returns the number of admitted endpoints left.
    pub fn sweep(&self, now: u64) -> usize {
        let today = now / 86400;
        let mut changed = false;
        let n = {
            let mut a = self.admissions.lock().unwrap();
            let before = a.len();
            a.retain(|_, until| *until > now);
            changed |= a.len() != before;
            a.len()
        };
        {
            let mut m = self.moved.lock().unwrap();
            let before = m.len();
            m.retain(|_, note| note.stored + MOVED_DAYS * 86400 > now);
            changed |= m.len() != before;
        }
        {
            let mut u = self.usage.lock().unwrap();
            for days in u.values_mut() {
                let before = days.len();
                days.retain(|d, _| d + USAGE_DAYS > today);
                changed |= days.len() != before;
            }
            u.retain(|_, days| !days.is_empty());
        }
        if changed {
            self.mark();
        }
        n
    }

    // Admissions.

    /// Admits `endpoints` until `until`, never shortening an admission. `Err` if the table is full.
    pub fn admit(&self, endpoints: &[[u8; 32]], until: u64, max: usize) -> Result<(), Full> {
        let mut a = self.admissions.lock().unwrap();
        let new = endpoints.iter().filter(|e| !a.contains_key(*e)).count();
        if a.len() + new > max {
            return Err(Full);
        }
        for e in endpoints {
            let slot = a.entry(*e).or_insert(0);
            *slot = (*slot).max(until);
        }
        drop(a);
        self.mark();
        Ok(())
    }

    pub fn is_admitted(&self, endpoint: &[u8; 32], now: u64) -> bool {
        self.admissions
            .lock()
            .unwrap()
            .get(endpoint)
            .is_some_and(|u| *u > now)
    }

    pub fn admitted_count(&self) -> usize {
        self.admissions.lock().unwrap().len()
    }

    // Moved notes.

    pub fn set_moved(&self, hub: &str, note: MovedNote) {
        self.moved.lock().unwrap().insert(hub.to_string(), note);
        self.mark();
    }

    pub fn moved(&self, hub: &str, now: u64) -> Option<MovedNote> {
        self.moved
            .lock()
            .unwrap()
            .get(hub)
            .filter(|n| n.stored + MOVED_DAYS * 86400 > now)
            .cloned()
    }

    // Usage.

    pub fn add_usage(&self, hub: &str, now: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut u = self.usage.lock().unwrap();
        *u.entry(hub.to_string())
            .or_default()
            .entry(now / 86400)
            .or_insert(0) += bytes;
        drop(u);
        self.mark();
    }

    /// Bytes relayed for `hub` in the last 30 days.
    pub fn usage(&self, hub: &str, now: u64) -> u64 {
        let today = now / 86400;
        self.usage
            .lock()
            .unwrap()
            .get(hub)
            .map(|d| {
                d.iter()
                    .filter(|(day, _)| *day + USAGE_DAYS > today)
                    .map(|(_, b)| *b)
                    .sum()
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdentityKey;

    fn record(key: &IdentityKey, ts: u64, packet: &[u8]) -> Vec<u8> {
        let sig = key.sign(&signable(ts, packet)).unwrap();
        [sig.as_slice(), &ts.to_be_bytes(), packet].concat()
    }

    #[test]
    fn records_order_and_expire() {
        let k = IdentityKey::ed25519_from_seed(&[3; 32]).unwrap();
        let pk: [u8; 32] = k.public_raw().try_into().unwrap();
        let r = Records::default();
        let a = record(&k, 10, b"\0\0\0\0\0\0\0\0\0\0\0\0");
        assert_eq!(r.put(&pk, &a, 100, 10), Ok(()));
        assert_eq!(r.put(&pk, &a, 100, 10), Ok(()), "same record again is fine");
        assert_eq!(
            r.put(&pk, &record(&k, 9, b"x"), 100, 10),
            Err(PutError::Older)
        );
        assert_eq!(
            r.put(&pk, &record(&k, 10, b"y"), 100, 10),
            Err(PutError::SameTimestamp)
        );
        let mut bad = record(&k, 11, b"z");
        bad[0] ^= 1;
        assert_eq!(r.put(&pk, &bad, 100, 10), Err(PutError::Bad));
        assert_eq!(
            r.put(&pk, &record(&k, 11, &[0; 1001]), 100, 10),
            Err(PutError::Bad)
        );
        assert!(r.get(&pk, 150, 100).is_some());
        assert!(r.get(&pk, 200, 100).is_none());
        assert_eq!(r.sweep(200, 100), 0);
        let other: [u8; 32] = IdentityKey::ed25519_from_seed(&[4; 32])
            .unwrap()
            .public_raw()
            .try_into()
            .unwrap();
        assert_eq!(r.put(&pk, &a, 100, 1), Ok(()));
        let k2 = IdentityKey::ed25519_from_seed(&[4; 32]).unwrap();
        assert_eq!(
            r.put(&other, &record(&k2, 1, b""), 100, 1),
            Err(PutError::Full)
        );
    }

    #[test]
    fn state_persists() {
        let dir = tempfile::tempdir().unwrap();
        let s = State::load(dir.path()).unwrap();
        s.admit(&[[1; 32]], 1000, 10).unwrap();
        s.admit(&[[1; 32]], 500, 10).unwrap();
        s.add_usage("h", 86400 * 5, 7);
        s.set_moved(
            "h",
            MovedNote {
                spki: "s".into(),
                body: "b".into(),
                sig: "g".into(),
                stored: 100,
            },
        );
        s.save().unwrap();
        let t = State::load(dir.path()).unwrap();
        assert!(t.is_admitted(&[1; 32], 999));
        assert!(!t.is_admitted(&[1; 32], 1000));
        assert_eq!(t.usage("h", 86400 * 5), 7);
        assert_eq!(t.usage("h", 86400 * 40), 0);
        assert!(t.moved("h", 200).is_some());
        assert!(s.admit(&[[2; 32], [3; 32]], 1000, 2).is_err());
        let mode = std::fs::metadata(dir.path().join(STATE_FILE)).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    }
}

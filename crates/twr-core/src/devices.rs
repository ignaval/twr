use anyhow::{Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn secret() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).expect("OS entropy");
    hex::encode(bytes)
}
pub fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub fn matches(hash: &str, value: &str) -> bool {
    bool::from(hash.as_bytes().ct_eq(digest(value).as_bytes()))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub created: u64,
    pub token_hash: String,
    pub scopes: Vec<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Database {
    devices: Vec<Device>,
    #[serde(default)]
    pairing: Option<Pairing>,
}
#[derive(Serialize, Deserialize)]
struct Pairing {
    hash: String,
    expires: u64,
    attempts: u32,
    scopes: Vec<String>,
}
pub struct Auth {
    path: PathBuf,
}
impl Auth {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let meta = std::fs::symlink_metadata(dir)?;
        anyhow::ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
            "Gateway data must be a directory owned by your user"
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            path: dir.join("devices.json"),
        })
    }
    fn lock(&self) -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path.with_extension("lock"))?;
        file.lock()?;
        Ok(file)
    }
    pub fn begin(&self, scopes: Vec<String>) -> Result<String> {
        let _lock = self.lock()?;
        anyhow::ensure!(
            scopes
                .iter()
                .all(|s| ["read", "operate", "terminal", "admin"].contains(&s.as_str())),
            "Unknown device scope"
        );
        let mut bytes = [0u8; 10];
        SystemRandom::new().fill(&mut bytes).expect("OS entropy");
        let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let code: String = bytes
            .into_iter()
            .map(|b| alphabet[(b & 31) as usize] as char)
            .collect();
        let mut db = self.read()?;
        db.pairing = Some(Pairing {
            hash: digest(&code),
            expires: now() + 300,
            attempts: 0,
            scopes,
        });
        self.save(&db)?;
        Ok(code)
    }
    fn read(&self) -> Result<Database> {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Database::default()),
            Err(e) => return Err(e.into()),
            Ok(m) => anyhow::ensure!(
                m.is_file()
                    && !m.file_type().is_symlink()
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.mode() & 0o077 == 0,
                "Unsafe gateway credentials file"
            ),
        }
        serde_json::from_slice(&std::fs::read(&self.path)?).context("Reading paired devices")
    }
    fn save(&self, db: &Database) -> Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension(format!("{}.tmp", secret()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(db)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
    pub fn pair(&self, code: &str, name: &str) -> Result<(Device, String)> {
        let _lock = self.lock()?;
        let mut db = self.read()?;
        let valid = if let Some(p) = &mut db.pairing {
            p.attempts = p.attempts.saturating_add(1);
            p.attempts <= 5
                && now() < p.expires
                && matches(
                    &p.hash,
                    &if code.len() == 10 {
                        code.to_ascii_uppercase()
                    } else {
                        code.to_owned()
                    },
                )
        } else {
            false
        };
        if !valid {
            let had_pairing = db.pairing.is_some();
            if db
                .pairing
                .as_ref()
                .is_some_and(|p| p.attempts > 5 || now() >= p.expires)
            {
                db.pairing = None;
            }
            if had_pairing {
                self.save(&db)?;
            }
            anyhow::bail!("Pairing code invalid or expired");
        }
        anyhow::ensure!(
            !name.trim().is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
            "Enter a device name (1–80 characters)"
        );
        anyhow::ensure!(
            db.devices.len() < 32,
            "Revoke an old device before pairing another"
        );
        let token = secret();
        let device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().to_owned(),
            created: now(),
            token_hash: digest(&token),
            scopes: db.pairing.take().unwrap().scopes,
        };
        db.devices.push(device.clone());
        self.save(&db)?;
        Ok((device, token))
    }
    pub fn authenticate(&self, token: &str) -> Result<Device> {
        anyhow::ensure!(token.len() == 64, "Unauthorized");
        self.read()?
            .devices
            .into_iter()
            .find(|d| matches(&d.token_hash, token))
            .context("Unauthorized")
    }
    pub fn devices(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!(self.read()?.devices.into_iter().map(|d|serde_json::json!({"id":d.id,"name":d.name,"created":d.created,"scopes":d.scopes})).collect::<Vec<_>>()))
    }
    pub fn current(&self, device: &Device) -> bool {
        self.read().is_ok_and(|db| {
            db.devices
                .iter()
                .any(|d| d.id == device.id && d.token_hash == device.token_hash)
        })
    }
    pub fn revoke(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut db = self.read()?;
        db.devices.retain(|d| d.id != id);
        self.save(&db)
    }
}
impl Device {
    pub fn allows(&self, method: &str) -> bool {
        method_scope(method).is_some_and(|scope| self.scopes.iter().any(|s| s == scope))
    }
    pub fn allows_params(&self, method: &str, params: &serde_json::Value) -> bool {
        if !self.allows(method) {
            return false;
        }
        let request = if method == "thread.spawn" {
            &params["request"]
        } else {
            params
        };
        if matches!(method, "thread.new" | "thread.spawn")
            && request["mode"]
                .as_str()
                .is_some_and(|mode| !["manual", "plan", "read-only", "dontAsk"].contains(&mode))
        {
            return self.scopes.iter().any(|s| s == "admin");
        }
        true
    }
}
pub const METHOD_SCOPES: &[(&str, &[&str])] = &[
    (
        "read",
        &[
            "hello",
            "hub.info",
            "hub.glance",
            "control.subscribe",
            "thread.subscribe",
            "blob.get",
            "notify.looking",
            "thread.list",
            "thread.get",
            "thread.activity",
            "thread.events",
            "thread.transcript",
            "thread.spawn_status",
            "thread.queue",
            "attention.list",
            "usage.get",
            "usage.threads",
            "orchestrator.show",
            "orchestrator.watches",
            "context.read",
            "memory.list",
            "memory.read",
            "job.list",
            "job.log",
            "trash.list",
            "log.stats",
            "speech.status",
            "decisions.list",
            "control.read",
            "desktop.subscribe",
            "desktop.preferences",
            "desktop.retry_attention",
            "desktop.parse_slash",
            "desktop.slash_commands",
        ],
    ),
    (
        "terminal",
        &[
            "shell.run",
            "shell.cd",
            "thread.attach",
            "terminal.open",
            "job.start",
            "job.cancel",
        ],
    ),
    (
        "admin",
        &[
            "device.list",
            "device.revoke",
            "device.pair_begin",
            "device.pair_complete",
            "account.list",
            "account.status",
            "account.add",
            "account.remove",
            "config.show",
            "desktop.config.read",
            "desktop.config.set",
            "desktop.config.save",
            "hub.shutdown",
            "thread.set_mode",
            "orchestrator.use",
            "orchestrator.reset",
            "box.agents",
            "box.projects",
            "box.install",
            "box.restart",
            "box.token",
            "box.add",
        ],
    ),
    (
        "operate",
        &[
            "say",
            "suggest.next",
            "summary.thread",
            "thread.new",
            "thread.spawn",
            "thread.send",
            "thread.send_cancel",
            "thread.stop",
            "thread.park",
            "thread.resume",
            "thread.trim",
            "thread.compact",
            "thread.set_model",
            "thread.rename",
            "thread.delete",
            "attention.respond",
            "attention.dismiss",
            "attention.annotate",
            "message.withdraw",
            "message.send_now",
            "orchestrator.watch",
            "orchestrator.unwatch",
            "context.set",
            "memory.save",
            "memory.forget",
            "trash.restore",
            "trash.empty",
            "log.compact",
            "speech.request",
            "speech.control",
        ],
    ),
];
pub fn method_scope(method: &str) -> Option<&'static str> {
    METHOD_SCOPES
        .iter()
        .find_map(|(scope, methods)| methods.contains(&method).then_some(*scope))
}
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_is_one_use_and_revocation_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let a = Auth::open(dir.path()).unwrap();
        let code = a.begin(vec!["read".into(), "operate".into()]).unwrap();
        assert!(a.pair("wrong", "phone").is_err());
        let (d, t) = a.pair(&code, "phone").unwrap();
        assert_eq!(a.authenticate(&t).unwrap().id, d.id);
        assert!(a.pair(&code, "second").is_err());
        assert!(!d.allows("shell.run"));
        assert!(d.allows("thread.send"));
        a.revoke(&d.id).unwrap();
        assert!(a.authenticate(&t).is_err());
    }
}

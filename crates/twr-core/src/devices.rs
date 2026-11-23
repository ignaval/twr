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
/// The open invitation: its code only as a hash, and the code's `K_proof`
/// ([`crate::phone_pairing::proof_key`]) for the typed-pairing proofs. Neither is ever logged:
/// this type has no `Debug`.
#[derive(Serialize, Deserialize)]
struct Pairing {
    hash: String,
    expires: u64,
    attempts: u32,
    scopes: Vec<String>,
    /// `K_proof`, hex. Absent in an invitation an older twr began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proof_key: Option<String>,
}
impl Pairing {
    /// Whether the invitation still takes a code: not expired, attempts left.
    fn open(&self) -> bool {
        self.attempts < MAX_ATTEMPTS && now() < self.expires
    }
}
/// How long an invitation lasts, in seconds.
pub const PAIRING_SECONDS: u64 = 300;
/// The wrong codes an invitation takes before it closes.
const MAX_ATTEMPTS: u32 = 5;
/// What became of an invitation ([`Auth::invitation`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationState {
    /// It still takes its code.
    Open,
    /// Another invitation replaced it.
    Replaced,
    /// Used, cancelled, expired or out of attempts.
    Gone,
}
/// The hash a code is kept as: of the normalized code, so that `abcd-efgh-jk23` is the code
/// `ABCDEFGHJK23`; text that is no code is hashed as it is (and matches no invitation).
fn code_hash(code: &str) -> String {
    digest(&crate::phone_pairing::normalize_code(code).unwrap_or_else(|| code.to_owned()))
}
/// Removes `db`'s invitation if it is closed (expired, or out of attempts); whether it did.
fn close(db: &mut Database) -> bool {
    let closed = db.pairing.as_ref().is_some_and(|p| !p.open());
    if closed {
        db.pairing = None;
    }
    closed
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
    /// A new invitation for `scopes`, replacing any open one: its code (12 symbols,
    /// [`crate::phone_pairing::new_code`]), which is kept only as a hash beside its `K_proof`.
    /// Deriving `K_proof` takes Argon2id's time (64 MiB): call it off an async runtime's
    /// threads.
    pub fn begin(&self, scopes: Vec<String>) -> Result<String> {
        // An expired invitation's key goes now, whether or not the new one can be made.
        self.read_open()?;
        anyhow::ensure!(
            scopes
                .iter()
                .all(|s| ["read", "operate", "terminal", "admin"].contains(&s.as_str())),
            "Unknown device scope"
        );
        // Admin writes configuration, which can name commands the hub runs.
        anyhow::ensure!(
            !scopes.iter().any(|s| s == "admin") || scopes.iter().any(|s| s == "terminal"),
            "Admin access includes running commands; grant terminal access with it"
        );
        // The slow part (Argon2id) before the lock.
        let code = crate::phone_pairing::new_code();
        let key = crate::phone_pairing::proof_key(&code)?;
        let _lock = self.lock()?;
        let mut db = self.read()?;
        db.pairing = Some(Pairing {
            hash: digest(&code),
            expires: now() + PAIRING_SECONDS,
            attempts: 0,
            scopes,
            proof_key: Some(hex::encode(key)),
        });
        self.save(&db)?;
        Ok(code)
    }
    /// The pin proof ([`crate::phone_pairing::pin_proof`]) of the open invitation for `pin`
    /// (raw), `nonce` and `gateway_id`; `None` when no invitation is open or it has no proof
    /// key. Asking is no attempt at the code.
    pub fn proof(
        &self,
        pin: &[u8; 32],
        nonce: &[u8],
        gateway_id: &str,
    ) -> Result<Option<[u8; 32]>> {
        let db = self.read_open()?;
        let Some(pairing) = db.pairing.as_ref().filter(|p| p.open()) else {
            return Ok(None);
        };
        let Some(key) = pairing.proof_key.as_deref() else {
            return Ok(None);
        };
        let key: [u8; 32] = hex::decode(key)
            .ok()
            .and_then(|k| k.try_into().ok())
            .context("Damaged pairing state")?;
        crate::phone_pairing::pin_proof(&key, pin, nonce, gateway_id).map(Some)
    }
    /// What became of the invitation of `code`.
    pub fn invitation(&self, code: &str) -> Result<InvitationState> {
        let hash = code_hash(code);
        let state = match self.read()?.pairing {
            Some(p) if p.hash == hash && p.open() => InvitationState::Open,
            Some(p) if p.hash != hash => InvitationState::Replaced,
            _ => InvitationState::Gone,
        };
        if state != InvitationState::Open {
            self.read_open()?;
        }
        Ok(state)
    }
    /// How many devices are paired, and whether an invitation is open.
    pub fn summary(&self) -> Result<(usize, bool)> {
        let db = self.read_open()?;
        Ok((
            db.devices.len(),
            db.pairing.as_ref().is_some_and(Pairing::open),
        ))
    }
    /// Revokes every device and closes the invitation (`twr phone rekey`); how many devices
    /// were paired.
    pub fn revoke_all(&self) -> Result<usize> {
        let _lock = self.lock()?;
        let mut db = self.read()?;
        let count = db.devices.len();
        db.devices.clear();
        db.pairing = None;
        self.save(&db)?;
        Ok(count)
    }
    /// Cancel exactly this invitation; closing an old window cannot cancel a newer code.
    pub fn cancel_pairing(&self, code: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let mut db = self.read()?;
        let cancelled = db
            .pairing
            .as_ref()
            .is_some_and(|pairing| pairing.hash == code_hash(code));
        if cancelled {
            db.pairing = None;
        }
        // Another, expired, invitation goes too.
        let closed = close(&mut db);
        if cancelled || closed {
            self.save(&db)?;
        }
        Ok(cancelled)
    }
    /// The store, with a closed invitation (expired, or out of attempts) removed first, and its
    /// `K_proof` with it: a proof key never outlives its code in `devices.json`.
    fn read_open(&self) -> Result<Database> {
        let db = self.read()?;
        if !db.pairing.as_ref().is_some_and(|p| !p.open()) {
            return Ok(db);
        }
        let _lock = self.lock()?;
        let mut db = self.read()?;
        if close(&mut db) {
            self.save(&db)?;
        }
        Ok(db)
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
            // Only a code as `phone_pairing::normalize_code` reads it: separators and case
            // aside, 12 symbols of the alphabet.
            p.attempts <= MAX_ATTEMPTS
                && now() < p.expires
                && crate::phone_pairing::normalize_code(code).is_some_and(|c| matches(&p.hash, &c))
        } else {
            false
        };
        if !valid {
            // Its attempt counted; closed (expired or out of attempts), it goes, key and all.
            let had_pairing = db.pairing.is_some();
            close(&mut db);
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
    /// The device this token belongs to. `Ok(None)` means definitely no paired device has it
    /// (never paired, revoked or replaced); `Err` means the device list could not be read,
    /// which says nothing about the token and must not be reported as a revocation.
    pub fn authenticate(&self, token: &str) -> Result<Option<Device>> {
        if token.len() != 64 {
            return Ok(None);
        }
        Ok(self
            .read()?
            .devices
            .into_iter()
            .find(|d| matches(&d.token_hash, token)))
    }
    pub fn devices(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!(self.read()?.devices.into_iter().map(|d|serde_json::json!({"id":d.id,"name":d.name,"created":d.created,"scopes":d.scopes})).collect::<Vec<_>>()))
    }
    /// Whether `device` is still paired with the same credential. Only
    /// [`Standing::Revoked`] is a reason to disconnect it.
    pub fn current(&self, device: &Device) -> Standing {
        match self.read() {
            Ok(db)
                if db
                    .devices
                    .iter()
                    .any(|d| d.id == device.id && d.token_hash == device.token_hash) =>
            {
                Standing::Current
            }
            Ok(_) => Standing::Revoked,
            Err(_) => Standing::Unknown,
        }
    }
    pub fn revoke(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut db = self.read()?;
        db.devices.retain(|d| d.id != id);
        self.save(&db)
    }
}
/// A paired device's standing, from a fresh read of the device list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Still paired with the same credential.
    Current,
    /// Definitely gone: revoked, or paired again with another credential.
    Revoked,
    /// The device list could not be read (an I/O error, no free file descriptor, a damaged
    /// file). Nothing is known: keep the connection, refuse new work until it reads again.
    Unknown,
}
impl Standing {
    /// The standing of `device` through `auth`, which itself failed to open when `Err`.
    pub fn of(auth: Result<Auth>, device: &Device) -> Self {
        auth.map_or(Self::Unknown, |auth| auth.current(device))
    }
    pub fn revoked(self) -> bool {
        self == Self::Revoked
    }
}

/// The permission modes a device without `admin` may give a new thread (`thread.new` /
/// `thread.spawn` `mode`): every other one widens what the thread does without asking, and
/// choosing it is a mode change, which needs `admin`.
pub const OPERATE_MODES: &[&str] = &["manual", "plan", "read-only", "dontAsk"];

/// Whether `scopes` (a device's grant) include `scope`.
pub fn grants(scopes: &[String], scope: &str) -> bool {
    scopes.iter().any(|s| s == scope)
}

/// Whether runtime spec `spec` is a command line (`pty:<command>`, `acp:<command>`, or the bare
/// `pty` / `acp`), which runs a program of the caller's choosing on the host: a device needs
/// `terminal` to name one. The hub checks the rest of the rule (an agent runtime or a
/// configured preset only) against its configuration.
pub fn is_command_runtime(spec: &str) -> bool {
    let spec = spec.trim();
    ["pty", "acp"].iter().any(|kind| {
        spec.strip_prefix(kind)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
    })
}

impl Device {
    pub fn allows(&self, method: &str) -> bool {
        method_scope(method).is_some_and(|scope| self.has(scope))
    }
    /// Whether the device holds `scope`.
    pub fn has(&self, scope: &str) -> bool {
        grants(&self.scopes, scope)
    }
    /// [`Device::allows`], and the parameters that widen what a method does past its scope:
    /// a new thread's mode outside [`OPERATE_MODES`] and `outside_pin` need `admin`; a command
    /// line runtime ([`is_command_runtime`]) needs `terminal`. The hub applies the same rules,
    /// and the rest of them (presets, moves, modes as canonicalized), where it decides.
    pub fn allows_params(&self, method: &str, params: &serde_json::Value) -> bool {
        if !self.allows(method) {
            return false;
        }
        let request = if method == "thread.spawn" {
            &params["request"]
        } else {
            params
        };
        if !matches!(
            method,
            "thread.new" | "thread.spawn" | "thread.set_model" | "orchestrator.use"
        ) {
            return true;
        }
        if matches!(method, "thread.new" | "thread.spawn")
            && request["mode"]
                .as_str()
                .is_some_and(|mode| !OPERATE_MODES.contains(&mode))
            && !self.has("admin")
        {
            return false;
        }
        if request["outside_pin"].as_bool() == Some(true) && !self.has("admin") {
            return false;
        }
        if request["runtime"].as_str().is_some_and(is_command_runtime) && !self.has("terminal") {
            return false;
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
            "conversation.search",
            "thread.event_context",
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
            "loop.get",
            "loop.report",
            "loop.list",
            "loop.setup.get",
            "loop.settings.get",
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
            "device.pair_cancel",
            "device.pair_proof",
            "account.list",
            "account.status",
            "account.add",
            "account.remove",
            "config.show",
            "config.set",
            "desktop.config.read",
            "desktop.config.set",
            "desktop.config.save",
            "hub.shutdown",
            "thread.set_mode",
            "orchestrator.use",
            "orchestrator.reset",
            "orchestrator.access",
            "box.agents",
            "box.projects",
            "box.install",
            "box.restart",
            "box.token",
            "box.add",
            "thread.pull",
            "runner.manage",
            "runner.status",
            "runner.connect",
            "runner.recover",
            "runner.revoke",
            "runner.forget",
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
            "thread.upload",
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
            // Starting and controlling loops, answering their setup cards, and saving their
            // settings (the same choice the card saves).
            "loop.request",
            "loop.setup.update",
            "loop.settings.set",
            "loop.settings.reset",
            "loop.control",
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
    fn cancelling_an_invitation_never_cancels_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let first = auth.begin(vec!["read".into()]).unwrap();
        let second = auth.begin(vec!["read".into()]).unwrap();
        assert!(!auth.cancel_pairing(&first).unwrap());
        assert!(!auth.cancel_pairing("wrong").unwrap());
        assert!(auth.pair(&second, "reader").is_ok());
        let third = auth.begin(vec!["read".into()]).unwrap();
        assert!(auth.cancel_pairing(&third).unwrap());
        assert!(!auth.cancel_pairing(&third).unwrap());
        assert!(auth.pair(&third, "reader").is_err());
    }
    /// Codes are 12 symbols, taken in any case and with separators; the invitation keeps its
    /// `K_proof`, which gives proofs a phone checks, until the invitation closes, by pairing,
    /// cancelling, expiring or running out of attempts. Asking for a proof is no attempt.
    #[test]
    fn invitations_keep_their_proof_key_until_they_close() {
        use crate::phone_pairing::{display_code, pin_bytes, proof_key, verify_pin_proof};
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let (pin, nonce) = (pin_bytes(b"spki"), [9u8; 32]);
        assert!(
            auth.proof(&pin, &nonce, "gw").unwrap().is_none(),
            "no invitation"
        );
        let code = auth.begin(vec!["read".into()]).unwrap();
        assert_eq!(code.len(), 12);
        let stored = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!stored.contains(&code), "the code is kept only as a hash");
        let key = proof_key(&code).unwrap();
        assert!(stored.contains(&hex::encode(key)));
        for _ in 0..8 {
            let proof = auth.proof(&pin, &nonce, "gw").unwrap().unwrap();
            assert!(verify_pin_proof(&key, &pin, &nonce, "gw", &proof));
        }
        assert!(
            auth.proof(&pin, &[1u8; 16], "gw").is_err(),
            "a nonce is 32 bytes"
        );
        assert_eq!(auth.invitation(&code).unwrap(), InvitationState::Open);
        assert_eq!(auth.summary().unwrap(), (0, true));
        // Four wrong codes and the right one, displayed and in lower case.
        for _ in 0..4 {
            assert!(auth.pair("ABCDEFGHJK23", "phone").is_err());
        }
        let typed = display_code(&code).to_ascii_lowercase();
        let (device, _) = auth.pair(&typed, "phone").unwrap();
        assert_eq!(device.scopes, ["read"]);
        assert!(auth.proof(&pin, &nonce, "gw").unwrap().is_none());
        assert_eq!(auth.invitation(&code).unwrap(), InvitationState::Gone);
        assert_eq!(auth.summary().unwrap(), (1, false));
        let stored = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(
            !stored.contains(&hex::encode(key)),
            "removed with the pairing"
        );
        // Five wrong codes close an invitation, and its proofs with it.
        let code = auth.begin(vec!["read".into()]).unwrap();
        for _ in 0..5 {
            assert!(auth.pair("ABCD-EFGH-JK23", "phone").is_err());
        }
        assert!(auth.proof(&pin, &nonce, "gw").unwrap().is_none());
        assert!(auth.pair(&code, "phone").is_err());
        // Cancelling takes the displayed code too; a newer invitation replaces an older one.
        let first = auth.begin(vec!["read".into()]).unwrap();
        let second = auth.begin(vec!["read".into()]).unwrap();
        assert_eq!(auth.invitation(&first).unwrap(), InvitationState::Replaced);
        assert!(auth.cancel_pairing(&display_code(&second)).unwrap());
        assert!(auth.proof(&pin, &nonce, "gw").unwrap().is_none());
        assert_eq!(auth.invitation(&second).unwrap(), InvitationState::Gone);
        // Expired.
        let code = auth.begin(vec!["read".into()]).unwrap();
        let path = dir.path().join("devices.json");
        let mut db: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        db["pairing"]["expires"] = serde_json::json!(now() - 1);
        std::fs::write(&path, db.to_string()).unwrap();
        assert!(auth.proof(&pin, &nonce, "gw").unwrap().is_none());
        assert_eq!(auth.invitation(&code).unwrap(), InvitationState::Gone);
        assert!(auth.pair(&code, "phone").is_err());
        assert_eq!(auth.revoke_all().unwrap(), 1);
        assert_eq!(auth.summary().unwrap(), (0, false));
    }
    /// An expired invitation leaves `devices.json`, its `K_proof` with it, as soon as the store
    /// reads it: to begin another, prove, pair, ask about it, count or cancel.
    #[test]
    fn an_expired_invitation_takes_its_proof_key_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let path = dir.path().join("devices.json");
        let stored = || std::fs::read_to_string(&path).unwrap();
        let expire = || {
            let mut db: serde_json::Value = serde_json::from_str(&stored()).unwrap();
            db["pairing"]["expires"] = serde_json::json!(now() - 1);
            std::fs::write(&path, db.to_string()).unwrap();
        };
        let pin = crate::phone_pairing::pin_bytes(b"spki");
        type Read = Box<dyn Fn(&Auth, &str)>;
        let reads: [(&str, Read); 6] = [
            (
                "proof",
                Box::new(move |a, _| assert!(a.proof(&pin, &[0u8; 32], "gw").unwrap().is_none())),
            ),
            (
                "invitation",
                Box::new(|a, c| assert_eq!(a.invitation(c).unwrap(), InvitationState::Gone)),
            ),
            (
                "invitation of another code",
                Box::new(|a, _| {
                    assert_eq!(
                        a.invitation("ABCDEFGHJK23").unwrap(),
                        InvitationState::Replaced
                    )
                }),
            ),
            (
                "summary",
                Box::new(|a, _| assert_eq!(a.summary().unwrap(), (0, false))),
            ),
            (
                "cancel of another code",
                Box::new(|a, _| assert!(!a.cancel_pairing("ABCDEFGHJK23").unwrap())),
            ),
            (
                "pair",
                Box::new(|a, c| assert!(a.pair(c, "phone").is_err())),
            ),
        ];
        for (what, read) in reads {
            let code = auth.begin(vec!["read".into()]).unwrap();
            let key = hex::encode(crate::phone_pairing::proof_key(&code).unwrap());
            assert!(stored().contains(&key), "{what}");
            expire();
            read(&auth, &code);
            assert!(!stored().contains(&key), "{what} left the key");
            assert!(
                !stored().contains("\"pairing\":{"),
                "{what} left the pairing"
            );
        }
        // Beginning another removes the expired one even when it then fails.
        let code = auth.begin(vec!["read".into()]).unwrap();
        let key = hex::encode(crate::phone_pairing::proof_key(&code).unwrap());
        expire();
        assert!(auth.begin(vec!["admin".into()]).is_err());
        assert!(!stored().contains(&key));
        let code = auth.begin(vec!["read".into()]).unwrap();
        let key = hex::encode(crate::phone_pairing::proof_key(&code).unwrap());
        expire();
        auth.begin(vec!["read".into()]).unwrap();
        assert!(!stored().contains(&key));
        // An open one stays.
        assert_eq!(auth.summary().unwrap(), (0, true));
    }
    /// An invitation an older twr began, without a proof key, still pairs; it gives no proof.
    #[test]
    fn an_invitation_without_a_proof_key_gives_no_proof() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let code = "ABCDEFGHJK23";
        let db = serde_json::json!({"devices": [], "pairing": {
            "hash": digest(code), "expires": now() + 60, "attempts": 0, "scopes": ["read"]}});
        let path = dir.path().join("devices.json");
        std::fs::write(&path, db.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pin = crate::phone_pairing::pin_bytes(b"spki");
        assert!(auth.proof(&pin, &[0u8; 32], "gw").unwrap().is_none());
        assert!(auth.pair("abcd efgh jk23", "phone").is_ok());
    }
    #[test]
    fn admin_invitations_include_terminal_access() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let error = auth
            .begin(vec!["read".into(), "operate".into(), "admin".into()])
            .unwrap_err();
        assert!(error.to_string().contains("terminal"));
        let scopes = ["read", "operate", "terminal", "admin"].map(String::from);
        let code = auth.begin(scopes.to_vec()).unwrap();
        assert_eq!(auth.pair(&code, "owner").unwrap().0.scopes, scopes);
    }
    #[test]
    fn read_only_history_access_cannot_answer_or_send() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let code = auth.begin(vec!["read".into()]).unwrap();
        let (device, _) = auth.pair(&code, "reader").unwrap();
        assert!(device.allows("conversation.search"));
        assert!(device.allows("thread.event_context"));
        assert!(!device.allows("attention.respond"));
        assert!(!device.allows("thread.send"));
    }
    fn device(scopes: &[&str]) -> Device {
        Device {
            id: "d".into(),
            name: "phone".into(),
            created: 0,
            token_hash: String::new(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }
    #[test]
    fn command_line_runtimes_need_terminal_and_widening_parameters_admin() {
        use serde_json::json;
        let operate = device(&["read", "operate"]);
        let terminal = device(&["read", "operate", "terminal"]);
        let admin = device(&["read", "operate", "admin"]);
        for spec in [
            "pty:/bin/sh -c id",
            "acp:my-agent --flag",
            " acp:x",
            "pty",
            "acp",
        ] {
            for (method, params) in [
                ("thread.new", json!({"project": "p", "runtime": spec})),
                (
                    "thread.spawn",
                    json!({"operation_id": "o", "request": {"project": "p", "runtime": spec}}),
                ),
                ("thread.set_model", json!({"thread": "t", "runtime": spec})),
            ] {
                assert!(!operate.allows_params(method, &params), "{method} {spec}");
                assert!(!admin.allows_params(method, &params), "{method} {spec}");
                assert!(terminal.allows_params(method, &params), "{method} {spec}");
            }
        }
        for spec in ["claude-code", "codex:work", "my-preset", "acpx", "ptyhon"] {
            let params = json!({"project": "p", "runtime": spec});
            assert!(operate.allows_params("thread.new", &params), "{spec}");
        }
        let outside = json!({"thread": "t", "account": "work", "outside_pin": true});
        assert!(!operate.allows_params("thread.set_model", &outside));
        assert!(!terminal.allows_params("thread.set_model", &outside));
        assert!(admin.allows_params("thread.set_model", &outside));
        let outside = json!({"project": "p", "account": "work", "outside_pin": true});
        assert!(!operate.allows_params("thread.new", &outside));
        assert!(admin.allows_params("thread.new", &outside));
        let bypass = json!({"project": "p", "mode": "bypassPermissions"});
        assert!(!operate.allows_params("thread.new", &bypass));
        assert!(admin.allows_params("thread.new", &bypass));
        assert!(operate.allows_params("thread.new", &json!({"project": "p", "mode": "plan"})));
        // `orchestrator.use` is admin's, and a command line there needs terminal too.
        let acp = json!({"runtime": "acp:agent"});
        assert!(!admin.allows_params("orchestrator.use", &acp));
        assert!(device(&["admin", "terminal"]).allows_params("orchestrator.use", &acp));
    }
    #[test]
    fn pairing_is_one_use_and_revocation_is_immediate() {
        let dir = tempfile::tempdir().unwrap();
        let a = Auth::open(dir.path()).unwrap();
        let code = a.begin(vec!["read".into(), "operate".into()]).unwrap();
        assert!(a.pair("wrong", "phone").is_err());
        let (d, t) = a.pair(&code, "phone").unwrap();
        assert_eq!(a.authenticate(&t).unwrap().unwrap().id, d.id);
        assert!(a.pair(&code, "second").is_err());
        assert!(!d.allows("shell.run"));
        assert!(d.allows("thread.send"));
        assert_eq!(a.current(&d), Standing::Current);
        a.revoke(&d.id).unwrap();
        assert!(a.authenticate(&t).unwrap().is_none());
        assert_eq!(a.current(&d), Standing::Revoked);
    }
    #[test]
    fn an_unreadable_device_list_is_not_a_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let a = Auth::open(dir.path()).unwrap();
        let code = a.begin(vec!["read".into()]).unwrap();
        let (device, token) = a.pair(&code, "phone").unwrap();
        // A token of the wrong shape is unknown without reading anything.
        assert!(a.authenticate("short").unwrap().is_none());
        let path = dir.path().join("devices.json");
        let saved = std::fs::read(&path).unwrap();
        for broken in [b"{\"devices\":[".as_slice(), b"".as_slice()] {
            std::fs::write(&path, broken).unwrap();
            assert!(a.authenticate(&token).is_err());
            assert!(a.authenticate(&"0".repeat(64)).is_err());
            assert_eq!(a.current(&device), Standing::Unknown);
            assert!(!a.current(&device).revoked());
        }
        std::fs::write(&path, &saved).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(a.current(&device), Standing::Unknown);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Standing::of(Err(anyhow::anyhow!("no free descriptor")), &device),
            Standing::Unknown
        );
        assert_eq!(a.authenticate(&token).unwrap().unwrap().id, device.id);
        assert_eq!(a.current(&device), Standing::Current);
        assert!(a.authenticate(&"0".repeat(64)).unwrap().is_none());
    }
}

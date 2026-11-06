use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
struct Receipt {
    hash: String,
    created: u64,
    #[serde(default)]
    reply: Option<Value>,
}
pub struct Receipts {
    dir: PathBuf,
    counts: std::sync::Mutex<std::collections::HashMap<String, usize>>,
}
impl Receipts {
    pub fn new(dir: &Path) -> Result<Self> {
        let dir = dir.join("requests");
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let mut counts = std::collections::HashMap::new();
        for file in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
            let name = file.file_name();
            if let Some(device) = name
                .to_str()
                .and_then(|name| name.get(..36))
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            {
                *counts.entry(device.to_string()).or_default() += 1;
            }
        }
        Ok(Self {
            dir,
            counts: std::sync::Mutex::new(counts),
        })
    }
    fn path(&self, device: &str, id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(device)?;
        uuid::Uuid::parse_str(id)?;
        Ok(self.dir.join(format!("{device}-{id}.json")))
    }
    pub fn begin(
        &self,
        device: &str,
        id: &str,
        workspace: &str,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>> {
        let path = self.path(device, id)?;
        let hash = hex::encode(Sha256::digest(serde_json::to_vec(&json!([
            workspace, method, params
        ]))?));
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path);
        match existing {
            Ok(file) => {
                let receipt: Receipt = serde_json::from_reader(file)?;
                anyhow::ensure!(
                    receipt.hash == hash,
                    "Request ID was already used for a different operation"
                );
                return Ok(Some(receipt.reply.unwrap_or_else(||json!({"id":id,"error":"This operation was started but its outcome is unknown. Check the hub state before trying a new request."}))));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // A device must never consume another device's replay budget. Keep
        // recent receipts rather than evicting them and risking duplicate work.
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(device.to_string()).or_default();
        anyhow::ensure!(*count<8192,"This device's daily operation receipt limit was reached. Try again after older receipts expire.");
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(&serde_json::to_vec(&Receipt {
            hash,
            created: super::auth::now(),
            reply: None,
        })?)?;
        file.sync_all()?;
        *count += 1;
        Ok(None)
    }
    pub fn finish(&self, device: &str, id: &str, reply: &Value) -> Result<()> {
        let path = self.path(device, id)?;
        let mut receipt: Receipt = serde_json::from_reader(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?,
        )?;
        anyhow::ensure!(
            serde_json::to_vec(reply)?.len() <= 1024 * 1024,
            "Reply too large to cache; check operation state before retrying"
        );
        receipt.reply = Some(reply.clone());
        let temp = path.with_extension(format!("{}.tmp", super::auth::secret()));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec(&receipt)?)?;
        file.sync_all()?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
    pub fn prune(&self) -> Result<()> {
        for file in std::fs::read_dir(&self.dir)? {
            let file = file?;
            let meta = file.metadata()?;
            if meta.is_file()
                && meta.modified()?.elapsed().unwrap_or_default().as_secs() > 86400
                && std::fs::remove_file(file.path()).is_ok()
            {
                let name = file.file_name();
                if let Some(device) = name.to_str().and_then(|name| name.get(..36)) {
                    if let Some(count) = self.counts.lock().unwrap().get_mut(device) {
                        *count = count.saturating_sub(1);
                    }
                }
            }
        }
        Ok(())
    }
}
pub fn mutation(method: &str, params: &Value) -> bool {
    matches!(method, "config-save" | "config-set" | "terminal-open")
        || (method == "cli" && params["args"][0] == "ws" && params["args"][1] == "new")
        || (method == "rpc"
            && twr_core::devices::method_scope(params["method"].as_str().unwrap_or(""))
                .is_some_and(|scope| scope != "read"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_device_cannot_exhaust_another_devices_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let receipts = Receipts::new(dir.path()).unwrap();
        let a = uuid::Uuid::new_v4().to_string();
        let b = uuid::Uuid::new_v4().to_string();
        receipts.counts.lock().unwrap().insert(a.clone(), 8192);
        assert!(receipts
            .begin(
                &a,
                &uuid::Uuid::new_v4().to_string(),
                "default",
                "rpc",
                &json!({"method":"thread.send"})
            )
            .is_err());
        assert!(receipts
            .begin(
                &b,
                &uuid::Uuid::new_v4().to_string(),
                "default",
                "rpc",
                &json!({"method":"thread.send"})
            )
            .unwrap()
            .is_none());
        assert!(!mutation("rpc", &json!({"method":"unknown"})));
        assert!(!mutation("push-register", &json!({})));
    }
    #[test]
    fn mutation_is_claimed_before_execution_and_returns_the_same_reply() {
        let dir = tempfile::tempdir().unwrap();
        let receipts = Receipts::new(dir.path()).unwrap();
        let d = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(receipts
            .begin(&d, &id, "default", "rpc", &json!({"method":"thread.send"}))
            .unwrap()
            .is_none());
        assert!(receipts
            .begin(&d, &id, "default", "rpc", &json!({"method":"thread.send"}))
            .unwrap()
            .unwrap()["error"]
            .is_string());
        let reply = json!({"id":id,"result":{"ok":true}});
        receipts.finish(&d, &id, &reply).unwrap();
        assert_eq!(
            receipts
                .begin(&d, &id, "default", "rpc", &json!({"method":"thread.send"}))
                .unwrap()
                .unwrap(),
            reply
        );
        assert!(receipts
            .begin(&d, &id, "other", "rpc", &json!({"method":"thread.send"}))
            .is_err());
    }
}

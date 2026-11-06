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
    count: std::sync::atomic::AtomicUsize,
}
impl Receipts {
    pub fn new(dir: &Path) -> Result<Self> {
        let dir = dir.join("requests");
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let count = std::fs::read_dir(&dir)?.filter_map(Result::ok).count();
        Ok(Self {
            dir,
            count: std::sync::atomic::AtomicUsize::new(count),
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
        // Bound durable receipts across all devices. Reject before executing when
        // full; never evict a recent receipt and accidentally repeat a mutation.
        self.count
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| (n < 8192).then_some(n + 1),
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "Daily operation receipt limit reached. Try again after older receipts expire."
                )
            })?;
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
                let _ = self.count.fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| Some(n.saturating_sub(1)),
                );
            }
        }
        Ok(())
    }
}
pub fn mutation(method: &str, params: &Value) -> bool {
    matches!(
        method,
        "config-save" | "config-set" | "terminal-open" | "push-register"
    ) || (method == "cli" && params["args"][0] == "ws" && params["args"][1] == "new")
        || (method == "rpc"
            && twr_core::devices::method_scope(params["method"].as_str().unwrap_or(""))
                != Some("read"))
}
#[cfg(test)]
mod tests {
    use super::*;
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

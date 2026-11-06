use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, Mutex},
};

type Replies = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
pub struct Companion {
    child: Child,
    input: Mutex<ChildStdin>,
    replies: Replies,
    next: AtomicU64,
    alive: Arc<std::sync::atomic::AtomicBool>,
}
impl Companion {
    pub async fn start(
        binary: &Path,
        workspace: &str,
        device: &str,
        token: &str,
        events: mpsc::Sender<Value>,
    ) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .arg(workspace)
            .env("TWR_CLIENT_NAME", format!("mobile-{device}"));
        command
            .env_remove("TWR_DEVICE_TOKEN")
            .env_remove("TWR_MOBILE_TRANSPORT");
        if !token.is_empty() {
            command.env("TWR_MOBILE_TRANSPORT", "1");
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("Starting shared twr companion")?;
        let mut input = child.stdin.take().context("Companion input")?;
        if !token.is_empty() {
            let mut init = serde_json::to_vec(&json!({"device_token":token}))?;
            init.push(b'\n');
            input.write_all(&init).await?;
            input.flush().await?;
        }
        let stdout = child.stdout.take().context("Companion output")?;
        let replies: Replies = Arc::new(Mutex::new(HashMap::new()));
        let pending = replies.clone();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running = alive.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(if let Some(e) = value["error"].as_str() {
                            Err(anyhow::anyhow!(e.to_owned()))
                        } else {
                            Ok(value["result"].clone())
                        });
                    }
                } else if events.send(value).await.is_err() {
                    break;
                }
            }
            running.store(false, Ordering::SeqCst);
            pending.lock().await.clear();
            let _=events.send(json!({"type":"companion-closed","connected":false,"error":"Shared hub companion closed. Reconnect to resume."})).await;
        });
        Ok(Self {
            child,
            input: Mutex::new(input),
            replies,
            next: AtomicU64::new(0),
            alive,
        })
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.replies.lock().await.insert(id, tx);
        let mut line = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        line.push(b'\n');
        if let Err(e) = self.input.lock().await.write_all(&line).await {
            self.replies.lock().await.remove(&id);
            return Err(e.into());
        }
        let result = tokio::time::timeout(Duration::from_secs(660), rx).await;
        self.replies.lock().await.remove(&id);
        result
            .context(
                "No reply; the operation may have completed. Check its state before retrying.",
            )?
            .context("Hub connection closed")?
    }
}
impl Drop for Companion {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

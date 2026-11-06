use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, OwnedSemaphorePermit};

pub struct Terminal {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub exited: Arc<AtomicBool>,
    pub started: std::time::Instant,
    master: Box<dyn MasterPty + Send>,
    writer: tokio::sync::mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    history: Arc<Mutex<String>>,
    sequence: Arc<AtomicU64>,
}
impl Terminal {
    pub fn open(
        binary: &Path,
        args: &[String],
        cwd: &Path,
        title: String,
        workspace: String,
        events: mpsc::Sender<Value>,
        permits: Vec<OwnedSemaphorePermit>,
    ) -> Result<Self> {
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            cols: 100,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(binary);
        command.args(args);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let mut child = pair
            .slave
            .spawn_command(command)
            .context("Starting remote terminal")?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input, mut received) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        std::thread::spawn(move || {
            while let Some(bytes) = received.blocking_recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let id = uuid::Uuid::new_v4().to_string();
        let history = Arc::new(Mutex::new(String::new()));
        let sequence = Arc::new(AtomicU64::new(0));
        let exited = Arc::new(AtomicBool::new(false));
        let hid = id.clone();
        let buffer = history.clone();
        let seq = sequence.clone();
        let output = events.clone();
        let (reader_finished, output_drained) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            let mut pending = Vec::new();
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&bytes[..n]);
                let text = match std::str::from_utf8(&pending) {
                    Ok(text) => {
                        let s = text.to_owned();
                        pending.clear();
                        s
                    }
                    Err(e) if e.error_len().is_none() => {
                        let valid = e.valid_up_to();
                        let s = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        pending.drain(..valid);
                        s
                    }
                    Err(_) => {
                        let s = String::from_utf8_lossy(&pending).into_owned();
                        pending.clear();
                        s
                    }
                };
                let sequence = {
                    let mut h = buffer.lock().unwrap();
                    h.push_str(&text);
                    if h.len() > 2 * 1024 * 1024 {
                        let mut at = h.len() - 2 * 1024 * 1024;
                        while !h.is_char_boundary(at) {
                            at += 1
                        }
                        h.drain(..at);
                    }
                    seq.fetch_add(1, Ordering::SeqCst) + 1
                };
                if output
                    .blocking_send(
                        json!({"type":"terminal-data","id":hid,"data":text,"sequence":sequence}),
                    )
                    .is_err()
                {
                    break;
                }
            }
            let _ = reader_finished.send(());
        });
        let eid = id.clone();
        let ended = exited.clone();
        std::thread::spawn(move || {
            let _permits = permits;
            let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
            let _ = output_drained.recv_timeout(std::time::Duration::from_millis(250));
            ended.store(true, Ordering::SeqCst);
            let _ = events.blocking_send(json!({"type":"terminal-exit","id":eid,"exitCode":code}));
        });
        Ok(Self {
            id,
            title,
            workspace,
            master: pair.master,
            writer: input,
            killer,
            pid,
            history,
            sequence,
            exited,
            started: std::time::Instant::now(),
        })
    }
    pub fn summary(&self) -> Value {
        json!({"id":self.id,"title":self.title,"workspace":self.workspace,"exited":self.exited.load(Ordering::SeqCst)})
    }
    pub fn read(&self) -> Value {
        let h = self.history.lock().unwrap();
        json!({"data":*h,"sequence":self.sequence.load(Ordering::SeqCst)})
    }
    pub async fn write(&mut self, text: &str) -> Result<()> {
        anyhow::ensure!(text.len() <= 65536, "Terminal input too large");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.writer.send(text.as_bytes().to_vec()),
        )
        .await
        .context("Terminal input stalled; wait for the program to read it")?
        .context("Terminal input is closed")?;
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        anyhow::ensure!(
            (2..=1000).contains(&cols) && (1..=500).contains(&rows),
            "Invalid terminal size"
        );
        self.master.resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(group) = self.master.process_group_leader() {
            if group > 0 {
                unsafe {
                    libc::kill(-group, libc::SIGTERM);
                }
            }
        }
        if let Some(pid) = self.pid {
            unsafe {
                if libc::getpgid(pid as i32) == pid as i32 {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
        }
        if !self.exited.load(Ordering::SeqCst) {
            let _ = self.killer.kill();
        }
    }
}

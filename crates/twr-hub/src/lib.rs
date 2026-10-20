//! twr hub daemon: event store, control stream, socket API and adapter supervisors.
//!
//! - [`store`]: SQLite schema, append-only log, blobs, pending prompts
//! - [`hub`]: the single-writer core, turn dispatch, supervisors, subscriptions
//! - [`host`]: where runtimes run ([`host::ThreadHost`])
//! - [`remote`]: remote boxes (a runner's box as a [`host::ThreadHost`])
//! - [`ssh_box`]: the hub's ssh supervisor of each SSH box
//! - [`listener`]: the runner WebSocket listener (`hub.listen`, optional TLS)
//! - [`report`]: report summaries (the hub fallback report, trimming written reports)
//! - [`server`]: the Unix socket NDJSON API
//! - [`job_gate`]: the process a job's command starts as, until the hub lets it run

pub mod host;
pub mod hub;
pub mod job_gate;
pub mod listener;
pub mod remote;
pub mod report;
pub mod server;
pub mod ssh_box;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;

use std::path::PathBuf;

pub use hub::{
    local_agent, lock_opened, AgentJobBy, AttachGrant, Caller, Hub, HubOptions, HubState, JobStart,
    NewThread, Relay, SendOutcome, ShellRun, SwitchOptions, LOCK_FILE,
};
pub use job_gate::job_gate_main;

/// Runs a hub in the foreground: claims its data-directory lock, binds `socket` (refusing
/// if another hub answers), opens the database, serves until SIGINT/SIGTERM or
/// `hub.shutdown`, then stops every runtime and removes the socket.
///
/// With `hub.listen` set, runners connect on that address ([`listener`]); a bad listener
/// setting (a non-loopback address without TLS, unreadable TLS files) fails here, before the
/// hub starts.
///
/// [`HubOptions::boxes_lock`], when set, is released once the socket is bound.
pub async fn run(mut opts: HubOptions, socket: PathBuf) -> anyhow::Result<()> {
    let boxes_lock = opts.boxes_lock.take();
    let lock = hub::lock_data_dir(&opts.db_path, Some(&socket)).await?;
    twr_core::trace::mark("hub: data directory locked");
    let runners = listener::RunnerListener::bind(&opts.config.hub).await?;
    let listener = server::bind(&socket).await?;
    twr_core::trace::mark("hub: socket bound");
    // A command that edits boxes.toml from now on finds the socket, and asks this hub.
    drop(boxes_lock);
    // The orchestrator's MCP server connects back here.
    opts.socket.get_or_insert_with(|| socket.clone());
    tracing::info!(socket = %socket.display(), db = %opts.db_path.display(), "hub listening");
    let hub = match Hub::open_locked(opts, lock).await {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_file(&socket);
            return Err(e);
        }
    };
    // Kept until the hub has shut down: runner connections end with it.
    let _runners = runners.map(|l| l.serve(hub.clone()));
    server::serve(hub, listener, socket, true).await
}

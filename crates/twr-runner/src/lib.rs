//! twr runner: runs adapters on a box and connects outbound to a hub.
//!
//! - [`local`]: the box-local git every box does the same way ([`local::LocalGit`]); the hub
//!   uses it for its own box (`local`).
//! - [`lbox`]: [`LocalBox`], the runner's executor: adapters + git + the durable [`spool`],
//!   running the hub's operations ([`twr_core::runner::Op`]).
//! - [`spool`]: the SQLite spool (outbox with per-thread `rseq`, command results, the
//!   identity of running runtimes).
//! - [`reap`]: recognizing and killing runtimes that outlived the process that started them
//!   (shared with the hub).
//! - [`workdirs`]: working-directory reservations that keep worktree changes and runtime
//!   starts in the same directory apart (shared with the hub's own box).
//! - [`agents`]: what a box learns of its environment for its agents: the login shell's
//!   `PATH` its runtimes get, and its agent CLIs (`Op::Agents`, the inventory).
//! - [`client`]: the WebSocket connection loop behind `twr runner` ([`run`]).
//! - [`daemon`]: an SSH box's daemon (`twr runner --daemon`): its Unix socket, the requests
//!   made on it, and the hub's sealed connections it takes from it.
//! - [`relay`]: what the hub's `ssh` runs on an SSH box (`twr runner --stdio`): starts the
//!   daemon when needed, then relays bytes between ssh and the daemon's socket.
//!
//! The wire protocol is [`twr_core::runner`].

pub mod agents;
pub mod client;
pub mod daemon;
pub mod lbox;
pub mod local;
pub mod reap;
pub mod relay;
pub mod spool;
pub mod workdirs;

pub use agents::{BoxEnv, BOX_PROBE_ENV};
pub use client::{run, run_with, DaemonOptions, RunnerOptions};
pub use lbox::{LocalBox, LocalBoxOptions};

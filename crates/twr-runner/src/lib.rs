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
//! - [`client`]: the WebSocket connection loop behind `twr runner` ([`run`]).
//!
//! The wire protocol is [`twr_core::runner`].

pub mod client;
pub mod lbox;
pub mod local;
pub mod reap;
pub mod spool;
pub mod workdirs;

pub use client::{run, run_with, RunnerOptions};
pub use lbox::{LocalBox, LocalBoxOptions};

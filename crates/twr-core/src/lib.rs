//! Core types for twr, a control tower for parallel coding agents.
//!
//! - [`event`]: the event envelope and every event kind
//! - [`activity`]: what a thread is doing, in a few words (its activity line's phrases)
//! - [`blob`]: content-addressed blob store trait and the 4 KB payload cap
//! - [`reduce`]: pure reducers (thread views, registry, attention stack, stack order)
//! - [`adapter`]: the runtime adapter trait and its channel-based handle
//! - [`git`]: the box git trait (worktrees, snapshots) and the porcelain v2 parser
//! - [`load`]: how loaded the machine is (pressure stall information, the load average)
//! - [`paths`], [`config`]: filesystem layout and `config.toml`
//! - [`notify`]: which control events deserve a user notification
//! - [`proto`], [`client`]: the hub socket protocol and an async client
//! - [`runner`]: the hub <-> runner WebSocket protocol
//! - [`testkit`]: event-sequence builders for tests
//! - [`usage`]: subscription usage windows, their fold and how clients show them

pub mod activity;
pub mod adapter;
pub mod b64;
pub mod blob;
pub mod client;
pub mod config;
pub mod event;
pub mod git;
pub mod load;
pub mod notify;
pub mod paths;
pub mod proto;
pub mod reduce;
pub mod runner;
pub mod testkit;
pub mod usage;

/// twr's version: the package version, plus `+dev.<build>` for a build made from a checkout by
/// `docs/tools/dev-install.sh`, which sets `TWR_BUILD` (the commit, `.dirty` with uncommitted
/// changes) at compile time.
pub fn version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| match option_env!("TWR_BUILD") {
            Some(build) if !build.is_empty() => {
                format!("{}+dev.{build}", env!("CARGO_PKG_VERSION"))
            }
            _ => env!("CARGO_PKG_VERSION").to_string(),
        });
    &VERSION
}

pub use event::{Event, EventBody, Source, HUB_THREAD};

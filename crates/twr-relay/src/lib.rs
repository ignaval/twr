//! twr's relay (docs/relay-protocol.md): the protocol's pure parts and the hub client, always;
//! the server (`twr relay serve`) with the `server` feature.

pub mod admit;
pub mod b32;
pub mod client;
pub mod clienthello;
pub mod der;
pub mod keys;
pub mod mux;
pub mod names;
pub mod proto;
#[cfg(feature = "server")]
pub mod server;
pub mod tls;
pub mod url;

pub use keys::IdentityKey;
pub use url::RelayUrl;

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

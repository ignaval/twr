//! Machine-owned runner service and authenticated links between workspaces and runners.
//! This crate owns public discovery; the worker protocol remains end-to-end sealed.
pub mod bootstrap;
pub mod bundle;
pub mod cache;
pub mod identity;
pub mod locator;
pub mod network;
pub mod pairing;
pub mod power;
pub mod service;
pub mod source;
pub mod state;
pub mod wire;
pub mod worker;

pub const ALPN: &[u8] = b"twr/link/1";
pub const PKARR_RELAY: &str = "https://dns.iroh.link/pkarr";

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    use ring::rand::SecureRandom;
    let mut bytes = [0; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    Ok(bytes)
}

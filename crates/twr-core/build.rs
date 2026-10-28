//! `TWR_CLIENT_FOLD`: a hash of what [`client_state::ClientFold`] is folded and serialized by
//! (twr-core's sources, its manifest and the workspace's `Cargo.lock`), which names the fold a
//! hub's control snapshot was made with (`control.subscribe` with `snapshot`, its ack's
//! `snapshot_fold`). A client installs only a snapshot folded by its own code: another build's
//! reducers may fold the same items otherwise.
//!
//! (FNV-1a, 64 bits: it tells builds apart, nobody forges it; no dependency needed.)
//!
//! [`client_state::ClientFold`]: crate::client_state::ClientFold

use std::path::{Path, PathBuf};

/// The files under `dir`, recursively, in a fixed order.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let lock = manifest.join("../../Cargo.lock");
    let mut inputs = vec![manifest.join("Cargo.toml"), lock.clone()];
    files(&manifest.join("src"), &mut inputs);
    println!("cargo:rerun-if-changed={}", manifest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("Cargo.toml").display()
    );
    println!("cargo:rerun-if-changed={}", lock.display());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for p in &inputs {
        let rel = p.strip_prefix(&manifest).unwrap_or(p);
        feed(rel.to_string_lossy().as_bytes());
        feed(&[0]);
        feed(&std::fs::read(p).unwrap_or_default());
        feed(&[0]);
    }
    println!("cargo:rustc-env=TWR_CLIENT_FOLD={h:016x}");
}

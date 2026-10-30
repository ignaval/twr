//! `TWR_FOLD_SOURCES`: a hash of what the hub's fold is built from (the sources of the crates
//! it uses, their manifests, `Cargo.lock`, and the compiler, target and settings that built it),
//! which names the build a checkpoint of the folded state is for (`hub::checkpoint`) at no cost
//! when the hub starts. Two builds of the same sources with the same compiler and settings fold
//! alike; any change to them is another build.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The crates whose code the fold runs, by directory under `crates/`.
const CRATES: [&str; 6] = [
    "twr-core",
    "twr-adapters",
    "twr-decisions",
    "twr-mcp",
    "twr-runner",
    "twr-hub",
];

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
    let root = manifest.join("../..");
    let mut inputs = vec![root.join("Cargo.toml"), root.join("Cargo.lock")];
    for c in CRATES {
        let dir = root.join("crates").join(c);
        inputs.push(dir.join("Cargo.toml"));
        files(&dir.join("src"), &mut inputs);
        println!("cargo:rerun-if-changed={}", dir.join("src").display());
        println!(
            "cargo:rerun-if-changed={}",
            dir.join("Cargo.toml").display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.lock").display()
    );
    let mut h = Sha256::new();
    // The compiler (its standard library: Unicode tables, float formatting) and how it builds.
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = std::process::Command::new(&rustc)
        .arg("-vV")
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    h.update(&version);
    for var in [
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_CFG_TARGET_FEATURE",
        "CARGO_CFG_PANIC",
    ] {
        let v = std::env::var(var).unwrap_or_default();
        h.update((var.len() as u64).to_le_bytes());
        h.update(var.as_bytes());
        h.update((v.len() as u64).to_le_bytes());
        h.update(v.as_bytes());
    }
    // The package's own features, each `CARGO_FEATURE_<name>`.
    let mut features: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("CARGO_FEATURE_"))
        .collect();
    features.sort();
    h.update(features.join(",").as_bytes());
    for var in ["RUSTC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    for p in &inputs {
        // Each file by its place in the tree, then its bytes (a file missing counts too).
        let name = p
            .strip_prefix(&root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(p).unwrap_or_default();
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    let digest = h.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=TWR_FOLD_SOURCES={hex}");
}

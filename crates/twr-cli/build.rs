//! Capture the exact source tree accompanying this build for native linked workers.
use std::path::{Path, PathBuf};
fn main() {
    for input in [
        "src",
        "build.rs",
        "Cargo.toml",
        "../../crates",
        "../../compat",
        "../../Cargo.toml",
        "../../Cargo.lock",
        "../../rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
    println!("cargo:rerun-if-env-changed=TWR_WORKER_SOURCE_BUNDLE");
    println!("cargo:rerun-if-env-changed=TWR_WORKER_SOURCE_MANIFEST");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let source = runtime
        .block_on(async {
            let repository = twr_source::Repository(root.clone());
            if let Some(existing) = std::env::var_os("TWR_WORKER_SOURCE_BUNDLE") {
                std::fs::copy(existing, out.join("worker-source.bundle"))?;
                let descriptor: twr_source::Source = serde_json::from_slice(&std::fs::read(
                    std::env::var_os("TWR_WORKER_SOURCE_MANIFEST")
                        .ok_or_else(|| anyhow::anyhow!("native source manifest missing"))?,
                )?)?;
                return Ok::<_, anyhow::Error>(descriptor);
            }
            let bundle = repository
                .capture_bundle(&out.join("worker-source.bundle"))
                .await?;
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let compiler = std::process::Command::new(rustc)
                .arg("--version")
                .output()?;
            anyhow::ensure!(compiler.status.success(), "cannot record compiler");
            let release = String::from_utf8(compiler.stdout)?
                .split_whitespace()
                .nth(1)
                .ok_or_else(|| anyhow::anyhow!("compiler release missing"))?
                .to_string();
            Ok(twr_source::Source {
                bundle,
                compiler: release,
                profile: std::env::var("PROFILE").unwrap(),
                flags: std::env::var("CARGO_ENCODED_RUSTFLAGS")
                    .unwrap_or_default()
                    .split('\x1f')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
            })
        })
        .unwrap_or_else(|error| panic!("cannot capture linked-worker source: {error:#}"));
    std::fs::write(
        out.join("worker-source.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
}

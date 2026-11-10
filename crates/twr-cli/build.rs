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
    // A native worker build on a runner embeds the source it was built from: that must work.
    if let Some(existing) = std::env::var_os("TWR_WORKER_SOURCE_BUNDLE") {
        let descriptor = (|| {
            std::fs::copy(existing, out.join("worker-source.bundle"))?;
            let descriptor: twr_source::Source = serde_json::from_slice(&std::fs::read(
                std::env::var_os("TWR_WORKER_SOURCE_MANIFEST")
                    .ok_or_else(|| anyhow::anyhow!("native source manifest missing"))?,
            )?)?;
            Ok::<_, anyhow::Error>(descriptor)
        })()
        .unwrap_or_else(|error| panic!("cannot embed the native worker's source: {error:#}"));
        write(&out, Some(&descriptor));
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let captured = runtime.block_on(async {
        let bundle = twr_source::Repository(root.clone())
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
        Ok::<_, anyhow::Error>(twr_source::Source {
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
    });
    match captured {
        Ok(source) => write(&out, Some(&source)),
        // A checkout the capture refuses (a sparse checkout, skip-worktree or assume-unchanged
        // entries, more untracked files than a bundle holds) still builds. The build records
        // no source, and a runner of another OS or CPU says it has none to build.
        Err(error) => {
            println!(
                "cargo:warning=this twr build records no linked-worker source ({error:#}); \
                 runners of another OS or CPU cannot build a worker from it"
            );
            write(&out, None);
            // Index flags change no watched file: capture again once the index changes.
            if let Ok(git) = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["rev-parse", "--absolute-git-dir"])
                .output()
            {
                if git.status.success() {
                    let dir = String::from_utf8_lossy(&git.stdout).trim().to_string();
                    println!("cargo:rerun-if-changed={dir}/index");
                }
            }
        }
    }
}

/// Writes the manifest `--export-worker-source` prints before its bundle; with no source, an
/// empty manifest and bundle.
fn write(out: &Path, source: Option<&twr_source::Source>) {
    let manifest = source.map_or_else(Vec::new, |s| serde_json::to_vec(s).unwrap());
    if source.is_none() {
        std::fs::write(out.join("worker-source.bundle"), b"").unwrap();
    }
    std::fs::write(out.join("worker-source.json"), manifest).unwrap();
}

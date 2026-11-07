//! Shared-service updates have a fixed signing key and official release source.
//! Mirrors and the debug test key accepted by the interactive updater never apply here.
use anyhow::{ensure, Context, Result};
use serde_json::json;
use std::path::Path;
pub async fn fetch(slot: &Path) -> Result<()> {
    super::refuse_agent_action("update the shared runner service")?;
    let source = super::update_source(None, None).await?;
    let tag = source.tag.as_deref().context("release tag missing")?;
    if !newer(tag, env!("CARGO_PKG_VERSION")) {
        println!("{}", json!({"updated":false}));
        return Ok(());
    }
    let target = super::boxes::release_target(std::env::consts::OS, std::env::consts::ARCH)
        .context("unsupported service target")?;
    let (archive, _) =
        super::fetch_release_archive_from(tag, target, &super::RELEASE_KEY, None).await?;
    let temporary = super::PrivateDir::create("twr-service-release")?;
    let archive = temporary.write("service.tar.gz", &archive)?;
    let mut executable = None;
    for entry in ["twr", "./twr"] {
        let mut cmd = tokio::process::Command::new("tar");
        cmd.arg("-xOzf").arg(&archive).arg(entry);
        if let twr_adapters::process::Bounded::Exited(Some(0), bytes) =
            twr_adapters::process::run_bounded(
                cmd,
                std::time::Duration::from_secs(30),
                256 * 1024 * 1024,
                |_| false,
            )
            .await
        {
            executable = Some(bytes);
            break;
        }
    }
    let bytes = executable.context("signed release archive has no twr executable")?;
    twr_core::paths::ensure_private_dir(slot)?;
    let path = slot.join("twr");
    twr_link::state::atomic_write(&path, &bytes)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    let mut check = tokio::process::Command::new(&path);
    check.arg("--link-service-version");
    let check =
        twr_adapters::process::run_bounded(check, std::time::Duration::from_secs(10), 4096, |_| {
            false
        })
        .await;
    let twr_adapters::process::Bounded::Exited(Some(0), bytes) = check else {
        anyhow::bail!("signed release does not support the runner service")
    };
    let version: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        version["protocol"] == 1 && version["state_schema"] == 1,
        "signed release has an incompatible service control protocol"
    );
    println!(
        "{}",
        json!({"updated":true,"executable":path,"version":tag})
    );
    Ok(())
}
fn newer(candidate: &str, current: &str) -> bool {
    fn parse(value: &str) -> Option<Vec<u64>> {
        let v = value.trim_start_matches('v');
        if v.contains('-') {
            return None;
        }
        let values: Vec<_> = v
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if values.len() == 3 {
            Some(values)
        } else {
            None
        }
    }
    matches!((parse(candidate),parse(current)),(Some(a),Some(b)) if a>b)
}
#[cfg(test)]
mod tests {
    #[test]
    fn never_downgrades_or_selects_prereleases() {
        assert!(super::newer("v0.4.0", "0.3.0"));
        assert!(!super::newer("v0.3.0", "0.3.0"));
        assert!(!super::newer("v0.2.9", "0.3.0"));
        assert!(!super::newer("v0.4.0-rc.1", "0.3.0"));
    }
}

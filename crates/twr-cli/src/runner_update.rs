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
/// Whether release `candidate` is newer than `current`, for the shared service: both must be
/// stable releases (a prerelease is never picked, and a service on one is never moved).
fn newer(candidate: &str, current: &str) -> bool {
    let stable = |v: &str| !v.contains('-') && !v.contains('+');
    stable(candidate)
        && stable(current)
        && compare_versions(candidate, current) == Some(std::cmp::Ordering::Greater)
}

/// Orders two versions (`v0.4.0`, `0.4.0-rc.1`; a leading `v` is optional) by semantic
/// versioning's precedence: the numbers first, then a prerelease before its release
/// (`0.4.0-rc.1` < `0.4.0`), prereleases by their dot-separated identifiers (numbers
/// numerically and before words, which compare as ASCII; a longer list after its prefix).
/// Build metadata (`+...`) is ignored. `None` when either is not such a version.
pub(crate) fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    fn number(s: &str) -> Option<u64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    }
    fn parse(value: &str) -> Option<([u64; 3], Vec<&str>)> {
        let v = value.strip_prefix('v').unwrap_or(value);
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        let mut parts = core.split('.');
        let numbers = [
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        ];
        if parts.next().is_some() {
            return None;
        }
        let pre: Vec<&str> = match pre {
            None => Vec::new(),
            Some(pre) => {
                let ids: Vec<&str> = pre.split('.').collect();
                let valid = |id: &&str| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                };
                if !ids.iter().all(valid) {
                    return None;
                }
                ids
            }
        };
        Some((numbers, pre))
    }
    let (a_core, a_pre) = parse(a)?;
    let (b_core, b_pre) = parse(b)?;
    let ordering = a_core
        .cmp(&b_core)
        .then_with(|| match (a_pre.is_empty(), b_pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (x, y) in a_pre.iter().zip(&b_pre) {
                    let o = match (number(x), number(y)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => x.cmp(y),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a_pre.len().cmp(&b_pre.len())
            }
        });
    Some(ordering)
}
#[cfg(test)]
mod tests {
    use std::cmp::Ordering::{Equal, Greater, Less};
    #[test]
    fn never_downgrades_or_selects_prereleases() {
        assert!(super::newer("v0.4.0", "0.3.0"));
        assert!(!super::newer("v0.3.0", "0.3.0"));
        assert!(!super::newer("v0.2.9", "0.3.0"));
        assert!(!super::newer("v0.4.0-rc.1", "0.3.0"));
        assert!(!super::newer("v0.4.0", "0.4.0-rc.1"));
    }
    #[test]
    fn versions_order_by_semver_precedence() {
        let cmp = super::compare_versions;
        assert_eq!(cmp("v0.3.0", "0.3.0"), Some(Equal));
        assert_eq!(cmp("v0.3.1", "0.3.0"), Some(Greater));
        assert_eq!(cmp("v0.10.0", "0.9.9"), Some(Greater));
        assert_eq!(cmp("v1.0.0", "0.99.99"), Some(Greater));
        assert_eq!(cmp("v0.2.9", "0.3.0"), Some(Less));
        // A prerelease comes before its release, and after the release before it.
        assert_eq!(cmp("v0.4.0", "0.4.0-rc.1"), Some(Greater));
        assert_eq!(cmp("v0.4.0-rc.1", "0.4.0"), Some(Less));
        assert_eq!(cmp("v0.4.0-rc.1", "0.3.9"), Some(Greater));
        assert_eq!(cmp("v0.3.9", "0.4.0-rc.1"), Some(Less));
        // Prereleases by their identifiers.
        assert_eq!(cmp("v0.4.0-rc.2", "0.4.0-rc.1"), Some(Greater));
        assert_eq!(cmp("v0.4.0-rc.10", "0.4.0-rc.9"), Some(Greater));
        assert_eq!(cmp("v0.4.0-rc", "0.4.0-rc.1"), Some(Less));
        assert_eq!(cmp("v0.4.0-beta", "0.4.0-alpha"), Some(Greater));
        assert_eq!(cmp("v0.4.0-1", "0.4.0-alpha"), Some(Less));
        assert_eq!(cmp("v0.4.0+build.1", "0.4.0"), Some(Equal));
        for bad in [
            "",
            "v",
            "0.4",
            "0.4.0.1",
            "x.4.0",
            "0.4.0-",
            "0.4.0-rc..1",
            "0.4.0-r_c",
        ] {
            assert_eq!(cmp(bad, "0.4.0"), None, "{bad:?}");
        }
    }
}

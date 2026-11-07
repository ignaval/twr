//! macOS idle-sleep assertion while machine-wide runtime/build leases are held.
#[cfg(any(target_os = "macos", test))]
use std::os::fd::AsRawFd;
use std::path::Path;
#[derive(Default)]
pub struct Power {
    child: Option<tokio::process::Child>,
}
impl Power {
    pub fn update(&mut self, leases: &Path, allow_sleep: bool) {
        #[cfg(target_os = "macos")]
        {
            if self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                self.child.take();
            }
            let busy = !allow_sleep && busy(leases);
            if busy && self.child.is_none() {
                self.child = tokio::process::Command::new("/usr/bin/caffeinate")
                    .args(["-i", "-w", &std::process::id().to_string()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .ok();
            } else if !busy {
                self.child.take();
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (leases, allow_sleep);
        }
    }
    pub fn held(&self) -> bool {
        self.child.is_some()
    }
}
#[cfg(any(target_os = "macos", test))]
fn busy(root: &Path) -> bool {
    let Ok(Some(_gate)) = twr_runner::managed::try_admission_gate(root) else {
        return true;
    };
    for name in (0..64)
        .map(|i| format!("runtime-{i}.lock"))
        .chain(std::iter::once("build.lock".to_string()))
    {
        let file = match std::fs::File::open(root.join(&name)) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return true;
        }
    }
    false
}
#[cfg(test)]
mod tests {
    #[test]
    fn holds_only_for_live_leases() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir()?;
        let gate = twr_runner::managed::admission_gate(dir.path())?;
        assert!(super::busy(dir.path()));
        assert!(!twr_runner::managed::Managed::at_capacity(dir.path())?);
        drop(gate);
        let file = std::fs::File::create(dir.path().join("build.lock"))?;
        assert!(!super::busy(dir.path()));
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(super::busy(dir.path()));
        drop(file);
        assert!(!super::busy(dir.path()));
        let high = std::fs::File::create(dir.path().join("runtime-63.lock"))?;
        assert_eq!(
            unsafe { libc::flock(high.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(super::busy(dir.path()));
        drop(high);
        assert!(!super::busy(dir.path()));
        Ok(())
    }
}

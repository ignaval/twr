//! What the hub's one-shot CLI calls share: next-message suggestions (`hub::suggest`) and a
//! review loop's review runs (`hub::loops::review`). Each runs an agent's CLI once, in a
//! session and process group of its own that is killed with everything it started when the
//! call ends, however it ends ([`Group`]), and with an environment scrubbed of twr's tokens and
//! markers, a `TWR_AGENT` marker of its own in their place ([`scrub_env`]).

/// Kills process group `pgid` (SIGKILL): what is left of a one-shot call.
pub(super) fn kill_group(pgid: u32) {
    if let Ok(pgid) = i32::try_from(pgid) {
        if pgid <= 1 {
            return;
        }
        // SAFETY: kill(2) with a negative pid signals that process group; no memory is shared.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// A one-shot call's process group, killed when the call ends any other way (a timeout, an
/// error, the caller's future dropped); `kill_on_drop` alone reaches only its leader. Set the
/// group to `None` once the leader was reaped: its id may name another group by then.
pub(super) struct Group(pub(super) Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            kill_group(pgid);
        }
    }
}

/// Removes from `cmd`'s environment what makes a one-shot call anything but a program of the
/// user's own: twr's secrets and markers (a `TWR_AGENT` marker `marker` instead), and every
/// variable whose name starts with one of `prefixes` (an API key, token, endpoint or
/// provider, the hub's own session of the runtime), but `keep`.
pub(super) fn scrub_env(
    cmd: &mut tokio::process::Command,
    marker: &str,
    prefixes: &[&str],
    keep: &[&str],
) {
    twr_adapters::process::scrub(cmd);
    for (k, _) in std::env::vars_os() {
        let k8 = k.to_string_lossy();
        if prefixes.iter().any(|p| k8.starts_with(p)) && !keep.contains(&k8.as_ref()) {
            cmd.env_remove(&k);
        }
    }
    cmd.env_remove(twr_runner::reap::RUNTIME_GEN_ENV);
    cmd.env(super::AGENT_ENV, marker);
}

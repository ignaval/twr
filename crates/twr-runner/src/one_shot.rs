//! What one-shot agent CLI calls share, on the hub (next-message suggestions, `hub::suggest`)
//! and on every box (a loop's review runs, [`crate::loops::review`], and an ACP agent's,
//! [`crate::loops::acp_review`]). Each runs an agent's CLI once, in a session and process group
//! of its own that is killed with everything it started when the call ends, however it ends
//! ([`Group`]), and with an environment scrubbed of twr's tokens and markers, a `TWR_AGENT`
//! marker of its own in their place ([`scrub_env`]).

/// Kills process group `pgid` (SIGKILL): what is left of a one-shot call.
pub fn kill_group(pgid: u32) {
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
pub struct Group(pub Option<u32>);

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
pub fn scrub_env(
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
    cmd.env_remove(crate::reap::RUNTIME_GEN_ENV);
    cmd.env(crate::reap::AGENT_ENV, marker);
}

/// The names [`scrub_env`] removes of this process's environment (every variable whose name
/// starts with one of `prefixes`, but `keep`, and the runtime generation), for a one-shot
/// call an adapter starts with a list of names to remove (`SpawnConfig::env_remove`) instead
/// of a command; the adapter removes twr's secrets itself.
pub fn scrubbed_names(prefixes: &[&str], keep: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| prefixes.iter().any(|p| k.starts_with(p)) && !keep.contains(&k.as_str()))
        .collect();
    let gen = crate::reap::RUNTIME_GEN_ENV.to_string();
    if !out.contains(&gen) && !keep.contains(&gen.as_str()) {
        out.push(gen);
    }
    out
}

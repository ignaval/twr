//! What one-shot agent CLI calls share, on the hub (next-message suggestions, `hub::suggest`)
//! and on every box (a loop's review runs, [`crate::loops::review`], and an ACP agent's,
//! [`crate::loops::acp_review`]). Each runs an agent's CLI once, in a session and process group
//! of its own that is killed with everything it started when the call ends, however it ends
//! ([`Group`], and [`end_call`] for what left the group), and with an environment scrubbed
//! of twr's tokens and markers, a `TWR_AGENT` marker of its own in their place ([`scrub_env`]).

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

/// What a one-shot call whose process group is `group` (its leader's pid) started, read while
/// its leader still runs, for [`end_call`]: the leader's descendants by parent links, with
/// their start times and groups ([`crate::reap::descendants`]). Empty when it can't be read.
pub async fn call_tree(group: Option<u32>) -> Vec<(u32, u64, i64)> {
    match group {
        Some(pgid) => tokio::task::spawn_blocking(move || crate::reap::descendants(pgid))
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Ends a one-shot call whose process group is `group` (its leader's pid; `None` once the
/// leader was reaped) and whose `TWR_AGENT` marker is `marker`: its group (SIGKILL), and what
/// it started outside the group, which an agent's tool may do (Claude Code's Bash tool runs
/// each command in a session of its own, so a test suite or a server it started outlives the
/// group's kill, reparented to init):
///
/// - `tree` ([`call_tree`], read before the leader was asked to stop), each process killed
///   while it is still that process, whatever environment it shows (macOS 27 shows none of
///   Apple's own programs', `/bin/sh` and `/bin/zsh` among them);
/// - then every process that still carries the call's marker, exactly that marker (another
///   call's, such as a concurrent audit run's, is never touched): what left the tree before
///   it was read (its parent exited first).
///
/// Whether anything outside the group was signalled. Reads the process table (`ps` off
/// Linux) off the async runtime.
pub async fn end_call(group: Option<u32>, marker: &str, tree: Vec<(u32, u64, i64)>) -> bool {
    if let Some(pgid) = group {
        kill_group(pgid);
    }
    let outside: Vec<(u32, u64)> = tree
        .into_iter()
        .filter(|&(_, _, pgrp)| Some(pgrp) != group.map(i64::from))
        .map(|(pid, start, _)| (pid, start))
        .collect();
    let markers = if marker.is_empty() {
        Vec::new()
    } else {
        vec![marker.to_string()]
    };
    tokio::task::spawn_blocking(move || {
        let killed = !outside.is_empty() && crate::reap::kill_each(&outside);
        let reaped = !markers.is_empty() && crate::reap::reap_agent_marked(&markers);
        killed || reaped
    })
    .await
    .unwrap_or(false)
}

/// What [`end_call`] adds to a run's log when it ended anything outside the run's group.
pub const REAPED_LINE: &str = "twr: ended what the agent left running outside its process \
     group (a command its tool started in a session of its own)";

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

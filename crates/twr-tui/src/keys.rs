//! What the terminal lets the TUI tell apart, found out at startup: whether Shift-Enter
//! arrives apart from Enter, and what to tell the user when it does not.
//!
//! - The kitty keyboard protocol ([`crate::run_with`] asks the terminal): Shift-Enter arrives
//!   as such.
//! - Inside tmux (`TMUX` set), tmux answers for the terminal and does not speak the kitty
//!   protocol, but it passes Shift-Enter on as an extended key when its `extended-keys` option
//!   is on and the program asks for xterm's modifyOtherKeys ([`MODIFY_OTHER_KEYS_ON`]). Only
//!   when tmux sends it as `CSI 13;2u` ([`tmux_plan`]): crossterm reads that form, and drops
//!   the xterm form (`CSI 27;2;13~`) whole, so asking for that one would lose the key.
//! - Over mosh, its terminal answers for the user's and passes on neither the kitty protocol
//!   nor modifyOtherKeys, so the terminal sends Shift-Enter as it does unasked: a plain Enter,
//!   unless a key binding of its own makes it send something else. mosh passes the bytes of a
//!   key on unchanged (measured with mosh 1.4), so a terminal set up to send Alt-Enter (`ESC
//!   CR`) for Shift-Enter, as `twr terminal-setup` and Claude Code's `/terminal-setup` do,
//!   breaks the line over mosh too. Its local echo also predicts that typing inserts, moving
//!   whatever is right of the cursor on the input row (the box's border, the cards beside it)
//!   until the TUI's own frame arrives; `mosh --predict-overwrite` predicts in place instead.
//!   Both go in a notice ([`MOSH_NOTICE`]).

use std::time::Duration;

/// What the terminal tells apart, as far as the TUI knows at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys {
    /// Shift-Enter is known to arrive apart from Enter (the keyboard protocol is on).
    pub shift_enter: bool,
    /// What the status bar says once, at startup, about breaking lines here.
    pub notice: Option<&'static str>,
}

/// Asks tmux to pass modified keys on as extended keys (xterm's modifyOtherKeys, mode 1):
/// tmux then sends Shift-Enter as `CSI 13;2u` and Ctrl-Enter as `CSI 13;5u`, every other key
/// as before.
pub const MODIFY_OTHER_KEYS_ON: &str = "\x1b[>4;1m";
/// Turns modifyOtherKeys back off.
pub const MODIFY_OTHER_KEYS_OFF: &str = "\x1b[>4;0m";

/// The startup notice over mosh.
pub const MOSH_NOTICE: &str = "over mosh, Shift-Enter starts a new line once your terminal \
     sends it as Alt-Enter (twr terminal-setup); Ctrl-J always does · mosh --predict-overwrite \
     stops the input box's border jumping as you type";
/// The startup notice inside a tmux that would send Shift-Enter in a form twr cannot read.
pub const TMUX_NOTICE: &str = "tmux sends Shift-Enter in a form twr can't read: `set -s \
     extended-keys-format csi-u` fixes it · Ctrl-J starts a new line";

/// What to do inside tmux ([`tmux_plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxPlan {
    /// Ask for modifyOtherKeys: tmux will send Shift-Enter as `CSI 13;2u`.
    Request,
    /// tmux sends extended keys in the xterm form whatever the TUI asks (`always`):
    /// crossterm drops them, Shift-Enter does nothing. Say how to fix it.
    Unreadable,
    /// Ask for nothing: Shift-Enter arrives as Enter.
    Nothing,
}

/// What tmux said when asked for one of its options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxAnswer {
    /// The option's value.
    Value(String),
    /// tmux has no such option (an older tmux).
    NoSuchOption,
}

/// [`TmuxPlan`] for tmux's `extended-keys` and `extended-keys-format` options, as tmux
/// answered (`None`: no answer). Asked for only when tmux surely sends the `CSI u` form: the
/// format is `csi-u`, or tmux has no format option (before 3.5, it sent no other). Without an
/// answer, nothing is asked: a wrong guess would lose Shift-Enter.
pub fn tmux_plan(extended_keys: Option<&TmuxAnswer>, format: Option<&TmuxAnswer>) -> TmuxPlan {
    let Some(TmuxAnswer::Value(extended)) = extended_keys else {
        return TmuxPlan::Nothing;
    };
    let csi_u = match format {
        Some(TmuxAnswer::NoSuchOption) => true,
        Some(TmuxAnswer::Value(f)) => f == "csi-u",
        None => return TmuxPlan::Nothing,
    };
    match extended.as_str() {
        "on" | "always" if csi_u => TmuxPlan::Request,
        "always" => TmuxPlan::Unreadable,
        _ => TmuxPlan::Nothing,
    }
}

/// What `tmux show-options` said, from whether it succeeded, its output and its errors:
/// `None` for anything but a value or tmux saying it has no such option.
pub fn tmux_answer(success: bool, stdout: &str, stderr: &str) -> Option<TmuxAnswer> {
    if success {
        Some(TmuxAnswer::Value(stdout.trim().to_string()))
    } else if stderr.contains("invalid option") || stderr.contains("unknown option") {
        Some(TmuxAnswer::NoSuchOption)
    } else {
        None
    }
}

/// Runs `tmux args` against the tmux server `TMUX` names: whether it succeeded, its output
/// and its errors; `None` when it cannot run or does not finish within a second.
fn tmux(args: &[&str]) -> Option<(bool, String, String)> {
    let mut child = std::process::Command::new("tmux")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(_) => return None,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Its few lines fit the pipes: it finished without waiting for them to be read.
    let (mut out, mut err) = (String::new(), String::new());
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    std::io::Read::read_to_string(&mut child.stderr.take()?, &mut err).ok()?;
    Some((status.success(), out, err))
}

/// tmux's server option `name` (`tmux show-options -sv`), as tmux answered ([`tmux_answer`]).
pub fn tmux_option(name: &str) -> Option<TmuxAnswer> {
    let (success, out, err) = tmux(&["show-options", "-sv", name])?;
    tmux_answer(success, &out, &err)
}

/// The pids of the tmux clients showing the session of the pane `TMUX_PANE` names (none
/// outside tmux, or when tmux does not answer).
fn tmux_clients() -> Vec<u32> {
    let Some(pane) = std::env::var_os("TMUX_PANE") else {
        return Vec::new();
    };
    let Some(pane) = pane.to_str() else {
        return Vec::new();
    };
    match tmux(&["list-clients", "-t", pane, "-F", "#{client_pid}"]) {
        Some((true, out, _)) => out.lines().filter_map(|l| l.trim().parse().ok()).collect(),
        _ => Vec::new(),
    }
}

/// The command name and the parent pid in a `/proc/<pid>/stat` line (`pid (comm) state ppid
/// ...`; the name may hold spaces and parentheses, so it ends at the last `)`).
pub fn stat_parent(stat: &str) -> Option<(&str, u32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let ppid = stat
        .get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((comm, ppid))
}

/// Whether a process named `name` is `pid` or one of its ancestors, reading each process's
/// stat line with `read`. Stops at pid 1, at a process it cannot read, and after 64 steps.
pub fn has_ancestor(name: &str, pid: u32, read: impl Fn(u32) -> Option<String>) -> bool {
    let mut pid = pid;
    for _ in 0..64 {
        let Some(stat) = read(pid) else {
            return false;
        };
        let Some((comm, ppid)) = stat_parent(&stat) else {
            return false;
        };
        if comm == name {
            return true;
        }
        if ppid <= 1 || ppid == pid {
            return false;
        }
        pid = ppid;
    }
    false
}

/// Whether `mosh-server` is among the ancestors of `own`, or of one of the tmux `clients`
/// showing it: the tmux server is a daemon, so under `mosh -> tmux -> twr` only the client
/// has mosh above it.
pub fn mosh_above(own: u32, clients: &[u32], read: impl Fn(u32) -> Option<String>) -> bool {
    std::iter::once(own)
        .chain(clients.iter().copied())
        .any(|pid| has_ancestor("mosh-server", pid, &read))
}

/// Whether the TUI runs under mosh ([`mosh_above`]; Linux only, through `/proc`). A mosh
/// further away (on the machine of an ssh or of a tmux this one runs in) is not seen.
pub fn under_mosh() -> bool {
    let clients = if std::env::var_os("TMUX").is_some_and(|t| !t.is_empty()) {
        tmux_clients()
    } else {
        Vec::new()
    };
    mosh_above(std::process::id(), &clients, |pid| {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_is_asked_only_when_it_sends_a_form_crossterm_reads() {
        let v = |s: &str| TmuxAnswer::Value(s.to_string());
        let none = TmuxAnswer::NoSuchOption;
        let plan = |e: &TmuxAnswer, f: Option<&TmuxAnswer>| tmux_plan(Some(e), f);
        // tmux before 3.5 (no format option) and `csi-u`: `CSI 13;2u`.
        assert_eq!(plan(&v("on"), Some(&none)), TmuxPlan::Request);
        assert_eq!(plan(&v("always"), Some(&none)), TmuxPlan::Request);
        assert_eq!(plan(&v("on"), Some(&v("csi-u"))), TmuxPlan::Request);
        assert_eq!(plan(&v("always"), Some(&v("csi-u"))), TmuxPlan::Request);
        // The xterm form: never asked for; sent anyway with `always`, so say so.
        assert_eq!(plan(&v("on"), Some(&v("xterm"))), TmuxPlan::Nothing);
        assert_eq!(plan(&v("always"), Some(&v("xterm"))), TmuxPlan::Unreadable);
        // Off: tmux never passes Shift-Enter on.
        assert_eq!(plan(&v("off"), Some(&none)), TmuxPlan::Nothing);
        assert_eq!(plan(&v("off"), Some(&v("xterm"))), TmuxPlan::Nothing);
        // Review: no answer about the format (tmux timed out, failed) is no guess: a tmux 3.5
        // in the xterm form would lose Shift-Enter.
        assert_eq!(plan(&v("on"), None), TmuxPlan::Nothing);
        assert_eq!(plan(&v("always"), None), TmuxPlan::Nothing);
        assert_eq!(tmux_plan(None, Some(&v("csi-u"))), TmuxPlan::Nothing);
        assert_eq!(tmux_plan(Some(&none), Some(&none)), TmuxPlan::Nothing);
    }

    #[test]
    fn tmux_answers_are_values_missing_options_or_nothing() {
        assert_eq!(
            tmux_answer(true, "on\n", ""),
            Some(TmuxAnswer::Value("on".into()))
        );
        assert_eq!(
            tmux_answer(false, "", "invalid option: extended-keys-format\n"),
            Some(TmuxAnswer::NoSuchOption)
        );
        assert_eq!(
            tmux_answer(false, "", "unknown option: extended-keys-format\n"),
            Some(TmuxAnswer::NoSuchOption)
        );
        assert_eq!(
            tmux_answer(false, "", "no server running on /tmp/tmux-1000/default\n"),
            None
        );
        assert_eq!(tmux_answer(false, "", ""), None);
    }

    #[test]
    fn stat_lines_give_the_name_and_the_parent() {
        assert_eq!(
            stat_parent("1228 (mosh-server) S 1 1228 1228 0 -1 4194560"),
            Some(("mosh-server", 1))
        );
        // A name with spaces and parentheses ends at the last `)`.
        assert_eq!(stat_parent("42 (a (b) c) R 7 42 42"), Some(("a (b) c", 7)));
        assert_eq!(stat_parent("garbage"), None);
        assert_eq!(stat_parent("5 (x) S notanumber"), None);
    }

    #[test]
    fn mosh_is_found_among_the_ancestors() {
        let table = |procs: &'static [(u32, &'static str)]| {
            move |pid: u32| {
                procs
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .map(|(_, s)| (*s).to_string())
            }
        };
        // twr <- bash <- mosh-server <- init.
        let mosh: &[(u32, &str)] = &[
            (300, "300 (twr) S 200 0 0"),
            (200, "200 (bash) S 100 0 0"),
            (100, "100 (mosh-server) S 1 0 0"),
        ];
        assert!(has_ancestor("mosh-server", 300, table(mosh)));
        // Over ssh: no mosh-server up to init.
        let ssh: &[(u32, &str)] = &[
            (300, "300 (twr) S 200 0 0"),
            (200, "200 (bash) S 100 0 0"),
            (100, "100 (sshd) S 1 0 0"),
        ];
        assert!(!has_ancestor("mosh-server", 300, table(ssh)));
        // A process it cannot read, or one that is its own parent, ends the walk.
        let broken: &[(u32, &str)] = &[(300, "300 (twr) S 250 0 0")];
        assert!(!has_ancestor("mosh-server", 300, table(broken)));
        let cycle: &[(u32, &str)] = &[(300, "300 (twr) S 300 0 0")];
        assert!(!has_ancestor("mosh-server", 300, table(cycle)));
        // Review: mosh -> tmux -> twr. The tmux server is a daemon (its parent is init), so
        // only the client attached to the session has mosh above it.
        let tmux: &[(u32, &str)] = &[
            (300, "300 (twr) S 200 0 0"),
            (200, "200 (bash) S 150 0 0"),
            (150, "150 (tmux: server) S 1 0 0"),
            (420, "420 (tmux: client) S 410 0 0"),
            (410, "410 (bash) S 400 0 0"),
            (400, "400 (mosh-server) S 1 0 0"),
            (520, "520 (tmux: client) S 510 0 0"),
            (510, "510 (sshd) S 1 0 0"),
        ];
        assert!(!mosh_above(300, &[], table(tmux)));
        assert!(!mosh_above(300, &[520], table(tmux)));
        assert!(mosh_above(300, &[520, 420], table(tmux)));
    }
}

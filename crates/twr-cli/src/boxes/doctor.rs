//! `twr box doctor` and `twr box login`: what is wrong with a box and how to fix it, and an
//! agent CLI's login on a box, in the user's terminal.
//!
//! **Doctor** works without a hub and with the box offline. Each check is printed as it ends,
//! `✓` (fine), `!` (worth a look) or `✗` (failed), with the fix when there is one:
//!
//! - the hub's view, when a hub runs (`hub.info`, asked within [`HUB_WAIT`]): the box's state,
//!   last error, version and builds against the hub's, a pending switch, a refusal;
//! - ssh without prompting, as the hub runs it ([`super::batch_check`]), and in this terminal
//!   ([`super::ssh_here`], within [`PROBE_WAIT`]), which runs the hub's probe
//!   ([`ssh_box::probe_script`]: OS, CPU, data directory, the builds tree's safety, the
//!   builds there) and [`inspect_script`]'s own lines (the builds newest first, logind, git,
//!   free disk, the configured projects' repositories);
//! - on the box, with one twr build there ([`pick_build`]: the hub's when it is there, else
//!   the box's configured one, else the newest; the output says which): the box daemon's
//!   status line (`twr runner --stdio --status`, SSH boxes; on Linux with the daemon's active
//!   systemd unit, if any, for the logind check) and the agent probe (`twr runner
//!   --agents`: the login shell's `PATH`, each agent's path, version and login state), in a
//!   second ssh in this terminal, within [`BOX_SIDE_WAIT`] ([`box_side_script`]);
//! - on a Mac: whether someone is logged in at its screen (`launchctl print gui/<uid>`, exit
//!   113 only meaning no one is), and, only while the box is connected (`online`: the hub
//!   keeps what a daemon said after it is gone), how its daemon was started and Claude Code's
//!   login as the daemon sees it, asked again through the hub first (`box.agents` with
//!   `refresh`, within [`REFRESH_WAIT`]) ([`macos_checks`], [`daemon_view`]).
//!
//! What ssh prints is read as [`super::ssh_here`] reads it: stdout bounded
//! ([`ssh_box::SSH_OUTPUT_MAX`]), stderr shown cleaned and bounded; every value kept from it
//! is cut short ([`ssh_box::NOTE_MAX_BYTES`]), and everything printed goes through
//! `outln!` ([`crate::term::clean`]). The agent probe keeps no account detail, so none is
//! shown.
//!
//! **Login** checks that the caller is the user ([`super::refuse_agent_action`]) with a
//! terminal on stdin and stdout, picks the build as doctor does, then runs `twr runner
//! --login <agent>` with it over ssh with a terminal ([`ssh_box::terminal_ssh_args`]: the
//! forced options and the refusals of the box's words stay), with ssh's stdin, stdout and
//! stderr this terminal's. The box's twr resolves the agent as its runner would and `exec`s
//! its login command. Afterwards the agent's login state comes from the hub (`box.agents`
//! with `refresh`: the box daemon's own view) when one runs, else from the agent probe over
//! ssh.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde::Serialize;
use serde_json::{json, Value};
use twr_core::config::{BoxTransport, Config};
use twr_core::paths::{sh_word, Paths};
use twr_core::runner::{AgentInfo, AgentLogin, AgentsReport, LoginPathReport};
use twr_hub::ssh_box;

use super::{
    agent_name, agent_said, batch_check, connect, default_workspace_only, refuse_agent_action,
    same_target, ssh_here, ssh_program_name, target_label, HubBuild, PROBE_WAIT,
};

/// How long the running hub may take to say what it knows (`hub.info`).
const HUB_WAIT: Duration = Duration::from_secs(15);

/// How long the checks run on the box with its twr build may take, ssh's questions included:
/// the daemon's status (a few seconds at most) and the agent probe (the login shell 5 s,
/// then each agent's commands 10 s, at once).
const BOX_SIDE_WAIT: Duration = Duration::from_secs(90);

/// How long `twr box login` (and doctor, on a Mac) waits for the hub to have the box probe
/// its agents again (the hub waits 30 s for the box).
const REFRESH_WAIT: Duration = Duration::from_secs(45);

/// Longest box home directory [`parse_inspect`] keeps.
const HOME_MAX_BYTES: usize = 4096;

/// The line [`inspect_script`]'s own lines follow.
const DOCTOR_MARK: &str = "twr-doctor 1";

/// The line before the daemon's status in [`box_side_script`]'s output.
const STATUS_MARK: &str = "twr-doctor-status";

/// The line before the box daemon's active systemd units in [`box_side_script`]'s output.
const UNITS_MARK: &str = "twr-doctor-units";

/// What the units section says when the box has no `systemctl` ([`box_side_script`]).
const NO_SYSTEMCTL: &str = "no-systemctl";

/// The line before the agent probe's JSON in [`box_side_script`]'s output.
const AGENTS_MARK: &str = "twr-doctor-agents";

/// What starts the line ending each section of [`box_side_script`]'s output: the exit status
/// of its command follows.
const EXIT_MARK: &str = "twr-doctor-exit ";

/// Longest login shell or login-PATH error kept from the box's report.
const AGENT_TEXT_MAX: usize = 1024;

/// A check's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mark {
    /// Fine.
    Ok,
    /// Worth a look; not a failure.
    Warn,
    /// Failed: the exit status is 1.
    Fail,
}

impl Mark {
    fn sign(self) -> &'static str {
        match self {
            Mark::Ok => "✓",
            Mark::Warn => "!",
            Mark::Fail => "✗",
        }
    }
}

/// One check, as printed and in `--json`.
#[derive(Debug, Clone, Serialize)]
struct Check {
    /// What was checked: `hub`, `ssh_batch`, `ssh`, `box`, `build`, `daemon`, `logind`,
    /// `macos`, `login_shell`, `agent`, `git`, `disk`, `project`.
    check: &'static str,
    /// Which one (the agent's command, the project's name), for `agent` and `project`.
    item: Option<String>,
    result: Mark,
    message: String,
    /// What to do about it.
    fix: Option<String>,
}

impl Check {
    /// How the check is named in text.
    fn label(&self) -> String {
        let base = match self.check {
            "ssh_batch" => "ssh without prompting",
            "ssh" => "ssh in this terminal",
            "login_shell" => "login shell",
            "macos" => "macOS",
            other => other,
        };
        match (&self.item, self.check) {
            (Some(i), "agent") => i.clone(),
            (Some(i), _) => format!("{base} {i}"),
            (None, _) => base.to_string(),
        }
    }
}

/// Which build on the box runs the checks there, `twr box login` and `twr box project`
/// ([`pick_build`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BuildSource {
    /// The hub's own (the running hub's, else this twr's).
    Hub,
    /// The box's configured `build`.
    Configured,
    /// The newest build there.
    Newest,
}

impl BuildSource {
    pub(super) fn said(self) -> &'static str {
        match self {
            BuildSource::Hub => "the hub's build",
            BuildSource::Configured => "the box's configured build",
            BuildSource::Newest => "the newest build on the box",
        }
    }
}

/// The checks of one `twr box doctor`, printed as they are added (text) or at the end
/// (`--json`).
struct Report {
    name: String,
    json: bool,
    checks: Vec<Check>,
    build: Option<(String, BuildSource)>,
    agents: Option<Vec<AgentInfo>>,
    login_path: Option<LoginPathReport>,
}

impl Report {
    fn new(name: &str, json: bool) -> Report {
        Report {
            name: name.to_string(),
            json,
            checks: Vec::new(),
            build: None,
            agents: None,
            login_path: None,
        }
    }

    fn add(&mut self, check: &'static str, result: Mark, message: String, fix: Option<String>) {
        self.add_item(check, None, result, message, fix);
    }

    fn add_item(
        &mut self,
        check: &'static str,
        item: Option<&str>,
        result: Mark,
        message: String,
        fix: Option<String>,
    ) {
        let c = Check {
            check,
            item: item.map(str::to_string),
            result,
            message,
            fix,
        };
        if !self.json {
            outln!("{} {}: {}", c.result.sign(), c.label(), c.message);
            if let Some(f) = &c.fix {
                outln!("    fix: {f}");
            }
        }
        self.checks.push(c);
    }

    fn failed(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.result == Mark::Fail)
            .count()
    }

    /// Prints the JSON (`--json`) or the last line, and the exit status: 1 when a check
    /// failed.
    fn finish(self) -> anyhow::Result<ExitCode> {
        let failed = self.failed();
        if self.json {
            let v = json!({
                "box": self.name,
                "ok": failed == 0,
                "build": self.build.as_ref().map(|(id, source)| json!({"id": id, "source": source})),
                "login_path": self.login_path,
                "agents": self.agents,
                "checks": self.checks,
            });
            outln!("{}", serde_json::to_string_pretty(&v)?);
        } else if failed == 0 {
            outln!("box {}: nothing failed", self.name);
        } else {
            outln!(
                "box {}: {failed} check{} failed",
                self.name,
                if failed == 1 { "" } else { "s" }
            );
        }
        Ok(if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        })
    }
}

/// Says `what` on stderr (in text mode only: `--json` keeps quiet but for ssh itself).
fn progress(json: bool, what: &str) {
    if !json {
        errln!("{what}");
    }
}

/// What the running hub on `socket` says, asked within [`HUB_WAIT`]: never starts one.
pub(super) async fn running_hub(socket: &Path) -> HubBuild {
    match tokio::time::timeout(HUB_WAIT, HubBuild::of_running(socket)).await {
        Ok(h) => h,
        Err(_) => HubBuild::own(Some(format!(
            "it did not answer within {} s",
            HUB_WAIT.as_secs()
        ))),
    }
}

/// The repositories of the projects configured for box `name` in `config`: its
/// `[boxes.<name>.projects.*]`, and each `[projects.*]` with `box = "<name>"` that the box
/// does not configure itself (its `repo` is then the path on the box, as for a thread).
fn box_projects(config: &Config, name: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(b) = config.boxes.get(name) {
        for (p, bp) in &b.projects {
            out.push((p.clone(), bp.repo.display().to_string()));
        }
    }
    for (p, pc) in &config.projects {
        if pc.box_name.as_deref() == Some(name) && !out.iter().any(|(n, _)| n == p) {
            out.push((p.clone(), pc.repo.display().to_string()));
        }
    }
    out.sort();
    out
}

/// The script of the first ssh: the hub's probe ([`ssh_box::probe_script`], in a subshell of
/// its own: it may `exit`), then [`DOCTOR_MARK`], `home=<$HOME>` (when absolute and printable),
/// `order=<build>` for each directory of the builds tree, newest first, and on a Mac `gui=`
/// with the exit status of `launchctl print gui/<uid>` (0: someone is logged in at the Mac's
/// screen, so a GUI session exists, as the relay checks it). With `repos`
/// (doctor), also: `linger=` / `kill=` (Linux: logind's `Linger` of this user and
/// `KillUserProcesses`, as the relay reads them), `git=` (`git --version` on the PATH ssh
/// gives, as the box daemon's), `df=` (`df -Pk` of the data directory, or the nearest
/// directory above it that exists), and `project.<i>=` (`git`, `notgit` or `missing`) for each
/// repository of `repos`, in order. Each of those commands is bounded
/// by [`BOX_COMMAND_SECS`] ([`bounded_prelude`]): one that hangs on the box (a `df` of a dead
/// network mount) ends with doctor, not after it.
pub(super) fn inspect_script(repos: Option<&[String]>) -> String {
    inspect_script_bounded(repos, BOX_COMMAND_SECS)
}

/// How long each command doctor runs on the box may take, in seconds ([`inspect_script`]).
const BOX_COMMAND_SECS: u32 = 10;

/// The shell function `twr_b`, which runs its arguments as a command for at most `secs`
/// seconds: with `timeout` where the box has one that takes `timeout <secs> <command>` (GNU,
/// BusyBox), else (a Mac) in the background, killed by a watcher once `secs` pass. The watcher
/// is a subshell whose `sleep` is its own child: stopped (`TERM`), it ends that `sleep` and
/// reaps it before it exits, so stopping it leaves nothing running. It ends it with `kill -9`:
/// a `TERM` that reaches the `sleep` before it has exec'd (still a fork of the shell, with the
/// shell's handler) can be lost, and the watcher would then wait out the whole `sleep`. `twr_b` stops it and waits
/// for it as soon as the command has ended, before it returns; so the watcher's `kill -9`
/// comes only after its whole `sleep`, while the command has not ended (bar one ending at that
/// very moment). Its output goes to `/dev/null`, so a `$(...)` around `twr_b` does not wait
/// for it.
fn bounded_prelude(secs: u32) -> String {
    format!(
        "if timeout 5 true >/dev/null 2>&1; then\n\
         \x20 twr_b() {{ timeout {secs} \"$@\"; }}\n\
         else\n\
         \x20 twr_b() {{\n\
         \x20   \"$@\" &\n\
         \x20   twr_p=$!\n\
         \x20   (\n\
         \x20     trap 'case $! in \"$twr_p\") ;; *) kill -9 \"$!\"; wait \"$!\" ;; esac; exit 0' TERM\n\
         \x20     sleep {secs} &\n\
         \x20     wait \"$!\" && kill -9 \"$twr_p\"\n\
         \x20   ) >/dev/null 2>&1 &\n\
         \x20   twr_w=$!\n\
         \x20   wait \"$twr_p\"; twr_s=$?\n\
         \x20   kill \"$twr_w\" >/dev/null 2>&1; wait \"$twr_w\" 2>/dev/null\n\
         \x20   return \"$twr_s\"\n\
         \x20 }}\n\
         fi\n"
    )
}

/// [`inspect_script`] with each box-side command bounded by `secs` seconds.
fn inspect_script_bounded(repos: Option<&[String]>, secs: u32) -> String {
    let mut s = format!(
        "(\n{probe})\n\
         echo '{DOCTOR_MARK}'\n\
         case ${{HOME:-}} in *[[:cntrl:]]*) ;; /*) printf 'home=%s\\n' \"$HOME\" ;; esac\n\
         case ${{XDG_DATA_HOME:-}} in /*) d=$XDG_DATA_HOME ;; *) d=${{HOME:-}}/.local/share ;; esac\n\
         ls -t \"$d/twr/runner/builds\" 2>/dev/null | sed 's/^/order=/'\n",
        probe = ssh_box::probe_script()
    );
    s.push_str(&bounded_prelude(secs));
    s.push_str(
        "if [ \"$(uname -s 2>/dev/null)\" = Darwin ]; then\n\
         \x20 twr_b /bin/launchctl print \"gui/$(id -u)\" </dev/null >/dev/null 2>&1; echo \"gui=$?\"\n\
         fi\n",
    );
    let Some(repos) = repos else {
        s.push_str("exit 0\n");
        return s;
    };
    s.push_str(
        "if [ \"$(uname -s 2>/dev/null)\" = Linux ]; then\n\
         \x20 if command -v loginctl >/dev/null 2>&1; then echo \"linger=$(twr_b loginctl show-user \"$(id -u)\" -p Linger 2>/dev/null)\"; else echo 'linger=none'; fi\n\
         \x20 command -v busctl >/dev/null 2>&1 && echo \"kill=$(twr_b busctl get-property org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager KillUserProcesses 2>/dev/null)\"\n\
         fi\n\
         echo \"git=$(twr_b git --version 2>/dev/null | head -n 1)\"\n\
         p=$d; while [ -n \"$p\" ] && [ \"$p\" != / ] && [ ! -d \"$p\" ]; do p=$(dirname \"$p\"); done\n\
         echo \"df=$(twr_b df -Pk \"${p:-/}\" 2>/dev/null | tail -n 1)\"\n",
    );
    for (i, repo) in repos.iter().enumerate() {
        s.push_str(&format!(
            "r={}; if [ -d \"$r\" ]; then if twr_b git -C \"$r\" rev-parse --git-dir >/dev/null 2>&1; \
             then echo 'project.{i}=git'; else echo 'project.{i}=notgit'; fi; else echo \
             'project.{i}=missing'; fi\n",
            sh_word(repo)
        ));
    }
    s.push_str("exit 0\n");
    s
}

/// What the first ssh found ([`inspect_script`]).
#[derive(Debug, Default)]
pub(super) struct Inspected {
    pub(super) probe: ssh_box::Probe,
    /// The builds tree's directories, newest first (not all of them builds twr trusts).
    pub(super) newest_first: Vec<String>,
    /// The box's home directory (`$HOME`, when absolute and printable; `twr box project`).
    pub(super) home: Option<String>,
    /// logind's `Linger=yes|no` of this user (`none`: no `loginctl`), when asked (Linux).
    linger: Option<String>,
    /// `busctl`'s `KillUserProcesses` (`b true`), when there is `busctl` (Linux).
    kill: Option<String>,
    /// The exit status of `launchctl print gui/<uid>` (a Mac).
    gui: Option<String>,
    /// `git --version`, empty without git.
    git: Option<String>,
    /// `df -Pk`'s line of the data directory.
    df: Option<String>,
    /// Each project's repository: `git`, `notgit` or `missing`, by index.
    projects: Vec<Option<String>>,
}

/// Parses the first ssh's output `out` ([`inspect_script`]) with `projects` repositories
/// asked: the probe ([`ssh_box::parse_probe`]: an error without its mark or an OS), then the
/// lines after the last [`DOCTOR_MARK`] line, each value cut at [`ssh_box::NOTE_MAX_BYTES`].
pub(super) fn parse_inspect(out: &str, projects: usize) -> anyhow::Result<Inspected> {
    let probe = ssh_box::parse_probe(out)?;
    let mut ins = Inspected {
        probe,
        projects: vec![None; projects],
        ..Inspected::default()
    };
    let lines: Vec<&str> = out.lines().map(|l| l.trim_end_matches('\r')).collect();
    let Some(at) = lines.iter().rposition(|l| *l == DOCTOR_MARK) else {
        return Ok(ins);
    };
    let cut = |v: &str| -> String { v.trim().chars().take(ssh_box::NOTE_MAX_BYTES).collect() };
    for line in &lines[at + 1..] {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "order" if twr_core::valid_build_id(v) => ins.newest_first.push(v.to_string()),
            "linger" if ins.linger.is_none() => ins.linger = Some(cut(v)),
            "kill" if ins.kill.is_none() => ins.kill = Some(cut(v)),
            "gui" if ins.gui.is_none() => ins.gui = Some(cut(v)),
            "git" if ins.git.is_none() => ins.git = Some(cut(v)),
            "df" if ins.df.is_none() => ins.df = Some(cut(v)),
            // Whole (a path cut short would be another one), absolute and printable only.
            "home"
                if ins.home.is_none()
                    && v.starts_with('/')
                    && v.len() <= HOME_MAX_BYTES
                    && !v.chars().any(char::is_control) =>
            {
                ins.home = Some(v.to_string())
            }
            _ => {
                let i = k
                    .strip_prefix("project.")
                    .and_then(|i| i.parse::<usize>().ok());
                if let Some(slot) = i.and_then(|i| ins.projects.get_mut(i)) {
                    slot.get_or_insert_with(|| cut(v));
                }
            }
        }
    }
    Ok(ins)
}

/// The build the checks on the box (and `twr box login`) run (R1): the hub's (`hub`) when the
/// box has it, else the box's configured one (`configured`), else the newest the box has
/// (`newest_first`, the builds tree newest first; any it has when that order is not known).
/// Only builds the probe trusts (`has`). `None` when it has none.
pub(super) fn pick_build(
    has: &BTreeSet<String>,
    newest_first: &[String],
    hub: Option<&str>,
    configured: Option<&str>,
) -> Option<(String, BuildSource)> {
    if let Some(h) = hub.filter(|h| has.contains(*h)) {
        return Some((h.to_string(), BuildSource::Hub));
    }
    if let Some(c) = configured.filter(|c| has.contains(*c)) {
        return Some((c.to_string(), BuildSource::Configured));
    }
    newest_first
        .iter()
        .find(|b| has.contains(*b))
        .or_else(|| has.iter().next())
        .map(|b| (b.clone(), BuildSource::Newest))
}

/// The script of the second ssh: build `id` under `data` runs, each section between its mark
/// and an [`EXIT_MARK`] line with its exit status, stdin `/dev/null` and stderr dropped: with
/// `status` (the box's name and the hub's public key), `twr runner --stdio --status` (the
/// status line of the box daemon of that hub, or `no daemon`), then (a valid key, Linux) the
/// active systemd user services the relay names for that daemon ([`unit_prefix`]: `systemctl
/// --user list-units`, bounded by [`BOX_COMMAND_SECS`]; [`NO_SYSTEMCTL`] without
/// `systemctl`); with `agents`, `twr runner --agents` (one JSON line). Each `twr runner` is
/// bounded too ([`BOX_STATUS_SECS`], [`BOX_AGENTS_SECS`], [`bounded_prelude`]): one that hangs
/// on the box ends with doctor, not after it.
fn box_side_script(data: &str, id: &str, status: Option<(&str, &str)>, agents: bool) -> String {
    box_side_script_bounded(data, id, status, agents, BOX_STATUS_SECS, BOX_AGENTS_SECS)
}

/// How long `twr runner --stdio --status` may take on the box, in seconds
/// ([`box_side_script`]).
const BOX_STATUS_SECS: u32 = 20;

/// How long `twr runner --agents` may take on the box, in seconds ([`box_side_script`]): its
/// login shell (5 s) and each agent's commands (10 s, at once), and some.
const BOX_AGENTS_SECS: u32 = 40;

// All of the second ssh's commands end before doctor stops waiting for it.
const _: () = assert!(
    (BOX_STATUS_SECS + BOX_COMMAND_SECS + BOX_AGENTS_SECS) as u64 + 10 < BOX_SIDE_WAIT.as_secs()
);

/// [`box_side_script`] with its `twr runner` commands bounded by `status_secs` and
/// `agents_secs` seconds.
fn box_side_script_bounded(
    data: &str,
    id: &str,
    status: Option<(&str, &str)>,
    agents: bool,
    status_secs: u32,
    agents_secs: u32,
) -> String {
    let exe = sh_word(&ssh_box::build_path(data, id));
    let mut s = String::new();
    if let Some((name, key)) = status {
        s.push_str(&bounded_prelude(status_secs));
        s.push_str(&format!(
            "echo '{STATUS_MARK}'\n\
             twr_b {exe} runner --stdio {} --hub-key {} --status </dev/null 2>/dev/null\n\
             echo \"{EXIT_MARK}$?\"\n",
            sh_word(&format!("--box={name}")),
            sh_word(key),
        ));
        if let Some(prefix) = unit_prefix(name, key) {
            s.push_str(&bounded_prelude(BOX_COMMAND_SECS));
            s.push_str(&format!(
                "if [ \"$(uname -s 2>/dev/null)\" = Linux ]; then\n\
                 \x20 echo '{UNITS_MARK}'\n\
                 \x20 if command -v systemctl >/dev/null 2>&1; then\n\
                 \x20   twr_b systemctl --user list-units --type=service --state=active \
                 --no-legend --plain {} </dev/null 2>/dev/null\n\
                 \x20   echo \"{EXIT_MARK}$?\"\n\
                 \x20 else echo '{NO_SYSTEMCTL}'; echo \"{EXIT_MARK}0\"; fi\n\
                 fi\n",
                sh_word(&format!("{prefix}*")),
            ));
        }
    }
    if agents {
        s.push_str(&bounded_prelude(agents_secs));
        s.push_str(&format!(
            "echo '{AGENTS_MARK}'\n\
             twr_b {exe} runner --agents </dev/null 2>/dev/null\n\
             echo \"{EXIT_MARK}$?\"\n"
        ));
    }
    s.push_str("exit 0\n");
    s
}

/// The lines of section `mark` of [`box_side_script`]'s output `out`, and its exit status
/// (`None`: no exit line came, the command was cut short). `None` when the section is not
/// there.
fn section<'a>(out: &'a str, mark: &str) -> Option<(Option<i32>, Vec<&'a str>)> {
    let mut lines = out.lines().map(|l| l.trim_end_matches('\r'));
    lines.by_ref().find(|l| *l == mark)?;
    let mut got = Vec::new();
    for l in lines {
        if let Some(code) = l.strip_prefix(EXIT_MARK) {
            return Some((code.trim().parse().ok(), got));
        }
        got.push(l);
    }
    Some((None, got))
}

/// The agent probe's report in section `lines` (its last line that parses), filtered as the
/// probe filters (`twr_adapters::agents::kept_report`, as the hub keeps a box's report): a
/// box's twr of another version may say anything.
fn parse_agents(lines: &[&str]) -> Option<AgentsReport> {
    let mut report: AgentsReport = lines
        .iter()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())?;
    let cut = |s: &mut Option<String>| {
        if let Some(v) = s.as_mut() {
            *v = twr_adapters::agents::printable(v, AGENT_TEXT_MAX);
        }
    };
    report.agents = twr_adapters::agents::kept_report(std::mem::take(&mut report.agents));
    cut(&mut report.login_path.shell);
    cut(&mut report.login_path.error);
    Some(report)
}

/// Runs [`box_side_script`] on box `name` over ssh in this terminal, within [`BOX_SIDE_WAIT`]:
/// its output, or why there is none.
async fn run_box_side(name: &str, ssh: &str, script: &str) -> Result<String, String> {
    let remote = ssh_box::sh_c(script);
    let run = ssh_here(ssh, &remote, None);
    match tokio::time::timeout(BOX_SIDE_WAIT, run).await {
        Err(_) => Err(format!(
            "it did not finish within {} s",
            BOX_SIDE_WAIT.as_secs()
        )),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Ok(Ok((Some(0), out))) => Ok(out),
        Ok(Ok((code, _))) => Err(format!(
            "ssh to box {name} failed ({}): see ssh's message above",
            code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"))
        )),
    }
}

/// Why a command of build `id` that exited `code` gave nothing usable: an older twr does not
/// know the flag (clap's usage error, exit 2).
pub(super) fn box_side_failure(id: &str, flag: &str, code: Option<i32>) -> String {
    match code {
        Some(2) => format!("build {id} does not know `{flag}` (a twr older than this one)"),
        Some(c) => format!("`twr runner {flag}` of build {id} exited {c}"),
        None => format!("`twr runner {flag}` of build {id} did not finish"),
    }
}

/// The hub's public box key, when the hub has made one (it has connected to an SSH box):
/// box daemons are per hub key.
fn hub_key(paths: &Paths) -> anyhow::Result<Option<String>> {
    let file = paths.data_dir.join(ssh_box::BOX_KEY_FILE);
    if !file.exists() {
        return Ok(None);
    }
    Ok(Some(
        twr_core::sealed::HubKey::load_or_create(&file)?.public_hex(),
    ))
}

/// `twr box doctor` (see [`super::BoxCmd::Doctor`]).
pub(super) async fn doctor(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    json: bool,
) -> anyhow::Result<ExitCode> {
    default_workspace_only(paths, "doctor")?;
    twr_core::runner::validate_box_name(name)?;
    let Some(cfg) = config.boxes.get(name) else {
        bail!(
            "there is no box {name} in config.toml or boxes.toml (`twr box add {name} <ssh \
             destination>` adds one)"
        );
    };
    let ssh_transport = cfg.transport() == BoxTransport::Ssh;
    let mut r = Report::new(name, json);
    if !json {
        outln!(
            "box {name}: {}",
            if ssh_transport {
                "ssh (the hub connects to it)"
            } else {
                "token (its runner connects to the hub)"
            }
        );
    }

    let hub = running_hub(socket).await;
    hub_checks(&mut r, name, &hub, ssh_transport);

    let Some(ssh) = cfg.ssh.as_deref().filter(|s| !s.trim().is_empty()) else {
        r.add(
            "ssh",
            Mark::Warn,
            "the box has no ssh destination: only the hub's view was checked".into(),
            Some(format!(
                "set `ssh` in [boxes.{name}] (`me@host`) to have the box itself checked"
            )),
        );
        return r.finish();
    };

    progress(
        json,
        &format!("checking ssh to box {name} without prompting..."),
    );
    match batch_check(name, ssh).await {
        None => r.add(
            "ssh_batch",
            Mark::Ok,
            "works (the hub's ssh never prompts)".into(),
            None,
        ),
        Some(why) => r.add(
            "ssh_batch",
            // Only the hub's ssh to an SSH box needs it.
            if ssh_transport {
                Mark::Fail
            } else {
                Mark::Warn
            },
            why,
            Some(
                "the hub runs ssh without prompting (BatchMode): accept the box's host key \
                 with ssh once, and load your key into an ssh-agent the hub can reach \
                 (SSH_AUTH_SOCK), or use a key without a passphrase"
                    .into(),
            ),
        ),
    }

    let projects = box_projects(config, name);
    let repos: Vec<String> = projects.iter().map(|(_, r)| r.clone()).collect();
    progress(
        json,
        &format!("probing box {name} over ssh in this terminal ({ssh})..."),
    );
    let remote = ssh_box::sh_c(&inspect_script(Some(&repos)));
    let probing = ssh_here(ssh, &remote, None);
    let out = match tokio::time::timeout(PROBE_WAIT, probing).await {
        Err(_) => Err(format!(
            "it did not finish within {} s (a question of ssh's left unanswered, or a box that \
             does not answer)",
            PROBE_WAIT.as_secs()
        )),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Ok(Ok((Some(0), out))) => Ok(out),
        Ok(Ok((code, _))) => Err(format!(
            "it failed ({}): see ssh's message above",
            code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"))
        )),
    };
    let ins = match out.map(|o| parse_inspect(&o, repos.len())) {
        Ok(Ok(ins)) => ins,
        Ok(Err(e)) => {
            r.add("ssh", Mark::Fail, format!("probing the box: {e:#}"), None);
            return r.finish();
        }
        Err(why) => {
            r.add(
                "ssh",
                Mark::Fail,
                format!("{why}; nothing on the box was checked"),
                Some(format!(
                    "check that `ssh {ssh}` reaches the box, and its [boxes.{name}] ssh"
                )),
            );
            return r.finish();
        }
    };
    let probe = &ins.probe;
    r.add(
        "ssh",
        Mark::Ok,
        format!(
            "reaches the box: {} ({} {})",
            target_label(probe),
            probe.os,
            probe.arch
        ),
        None,
    );
    let Some(data) = box_checks(&mut r, name, probe) else {
        return r.finish();
    };

    // The build the checks on the box run (R1).
    let same = same_target(probe);
    let configured = cfg.build.as_deref();
    let pick = pick_build(
        &probe.has,
        &ins.newest_first,
        hub.build.as_deref(),
        configured,
    );
    build_check(
        &mut r,
        name,
        &hub,
        configured,
        same,
        ssh_transport,
        pick.as_ref(),
    );
    r.build = pick.clone();

    let key = if ssh_transport { hub_key(paths)? } else { None };
    let side = match &pick {
        Some((id, _)) => {
            progress(
                json,
                &format!("running build {id}'s checks on box {name} over ssh..."),
            );
            let status = key.as_deref().map(|k| (name, k));
            Some(run_box_side(name, ssh, &box_side_script(&data, id, status, true)).await)
        }
        None => None,
    };

    if ssh_transport {
        daemon_check(
            &mut r,
            name,
            &hub,
            configured,
            same,
            pick.as_ref(),
            key.is_some(),
            side.as_ref(),
        );
        let prefix = key.as_deref().and_then(|k| unit_prefix(name, k));
        let stop = match (&pick, key.as_deref()) {
            (Some((id, _)), Some(k)) => super::stop_command(ssh, id, name, k).ok(),
            _ => None,
        };
        logind_check(
            &mut r,
            probe,
            &ins,
            &daemon_launch(side.as_ref(), prefix.as_deref()),
            stop.as_deref(),
        );
    }
    let mac = probe.os == "Darwin";
    if mac {
        macos_checks(&mut r, name, &ins, &hub, ssh_transport);
    }
    let view = if mac {
        daemon_view(socket, &hub, name, json).await
    } else {
        DaemonView::NotAsked
    };
    agents_checks(
        &mut r,
        name,
        &hub,
        &view,
        pick.as_ref(),
        side.as_ref(),
        ssh_transport,
    );
    match ins.git.as_deref().filter(|g| !g.is_empty()) {
        Some(g) => r.add("git", Mark::Ok, g.to_string(), None),
        None => r.add(
            "git",
            Mark::Fail,
            "git is not on the PATH ssh gives on the box (the box daemon's)".into(),
            Some("install git on the box".into()),
        ),
    }
    disk_check(&mut r, config, &data, ins.df.as_deref());
    for (i, (project, repo)) in projects.iter().enumerate() {
        let (mark, said, fix) = match ins.projects.get(i).cloned().flatten().as_deref() {
            Some("git") => (Mark::Ok, format!("{repo} is a git repository"), None),
            Some("notgit") => (
                Mark::Fail,
                format!("{repo} is no git repository"),
                Some(format!(
                    "clone the project there, or point its `repo` at the repository on box \
                     {name}"
                )),
            ),
            Some("missing") => (
                Mark::Fail,
                format!("{repo} does not exist on the box"),
                Some(format!(
                    "clone the project to {repo} on box {name}, or set its `repo` \
                     ([boxes.{name}.projects.{project}]) to where it is there"
                )),
            ),
            _ => (Mark::Warn, format!("{repo}: not checked"), None),
        };
        r.add_item("project", Some(project), mark, said, fix);
    }
    r.finish()
}

/// The hub's view of box `name` (`hub`): its state, last error, versions and builds against
/// the hub's, a pending switch, a refusal.
fn hub_checks(r: &mut Report, name: &str, hub: &HubBuild, ssh_transport: bool) {
    if let Some(why) = &hub.unknown {
        r.add(
            "hub",
            Mark::Warn,
            format!("the hub did not say what it knows ({why}): its view is not checked"),
            None,
        );
        return;
    }
    if hub.info.is_none() {
        r.add(
            "hub",
            Mark::Warn,
            "no hub runs: its view is not checked".into(),
            Some("start it with `twr` (or `twr hub`)".into()),
        );
        return;
    }
    let Some(b) = hub.box_entry(name) else {
        r.add(
            "hub",
            Mark::Warn,
            format!("the running hub has no box {name} (it was added after the hub started)"),
            Some(
                "restart the hub (`twr hub stop`, then `twr`), or add it with `twr box add`".into(),
            ),
        );
        return;
    };
    let state = super::str_of(&b["state"]);
    let last_error = b["last_error"].as_str();
    let with_error = |s: String| match last_error {
        Some(e) => format!("{s}; last error: {e}"),
        None => s,
    };
    match state {
        "online" => {
            let version = b["version"].as_str().unwrap_or("an unknown version");
            let running = b["running_build"]
                .as_str()
                .or(b["build"].as_str())
                .map(|id| format!(", build {id}"))
                .unwrap_or_default();
            r.add(
                "hub",
                Mark::Ok,
                format!("box {name} is online: twr {version}{running}"),
                None,
            );
            if b["version"].as_str().is_some_and(|v| v != hub.version) {
                r.add(
                    "hub",
                    Mark::Warn,
                    format!(
                        "box {name} runs twr {version}, the hub twr {} (they speak the same \
                         runner protocol)",
                        hub.version
                    ),
                    ssh_transport.then(|| format!("`twr box install {name}` installs the hub's")),
                );
            }
        }
        "switching" => r.add(
            "hub",
            Mark::Warn,
            format!("box {name} is switching to the build the hub launches"),
            None,
        ),
        "connecting" => r.add(
            "hub",
            Mark::Warn,
            with_error(format!("the hub is connecting to box {name}")),
            None,
        ),
        "needs_install" => r.add(
            "hub",
            Mark::Fail,
            with_error(format!("box {name} needs a twr build")),
            Some(format!("`twr box install {name}`")),
        ),
        "error" => r.add(
            "hub",
            Mark::Fail,
            with_error(format!("the hub cannot connect to box {name}")),
            None,
        ),
        other => r.add(
            "hub",
            Mark::Fail,
            with_error(format!("box {name} is {other}")),
            (!ssh_transport).then(|| {
                "start the box's runner (`twr runner --hub ... --box ...`, docs/runners.md)"
                    .to_string()
            }),
        ),
    }
    if b["switch_pending"] == true {
        r.add(
            "hub",
            Mark::Warn,
            format!(
                "box {name} runs build {}, not build {}: the hub switches it once it has been \
                 quiet for 2 minutes",
                b["running_build"].as_str().unwrap_or("-"),
                b["build"].as_str().unwrap_or("-")
            ),
            None,
        );
    }
    if let Some(refused) = b["refused"].as_object() {
        r.add(
            "hub",
            Mark::Fail,
            format!(
                "the hub refused a connection of box {name} ({}): {}",
                super::super::trash_time(&refused["at"]),
                super::str_of(&refused["reason"])
            ),
            None,
        );
    }
}

/// The probe's findings: the data directory and the builds tree. The data directory, when the
/// checks on the box can go on.
fn box_checks(r: &mut Report, name: &str, probe: &ssh_box::Probe) -> Option<String> {
    let Some(data) = probe.data.clone() else {
        r.add(
            "box",
            Mark::Fail,
            "neither XDG_DATA_HOME nor HOME is an absolute path on the box".into(),
            Some("twr keeps its builds and data under it: set HOME for ssh's commands".into()),
        );
        return None;
    };
    if let Some(why) = probe.tree_refusal(name) {
        r.add("box", Mark::Fail, why, None);
        return None;
    }
    let builds = if probe.has.is_empty() {
        "none".to_string()
    } else {
        probe.has.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    r.add(
        "box",
        Mark::Ok,
        format!("data directory {data}, twr builds there: {builds}"),
        None,
    );
    if !probe.unsafe_paths.is_empty() {
        r.add(
            "box",
            Mark::Warn,
            format!("not trusted, left out: {}", probe.unsafe_paths.join("; ")),
            Some("make each of them yours, and writable by you alone (`chmod go-w`)".into()),
        );
    }
    Some(data)
}

/// Which build runs the checks on box `name`, and whether the hub's (or the configured one)
/// is there.
#[allow(clippy::too_many_arguments)]
fn build_check(
    r: &mut Report,
    name: &str,
    hub: &HubBuild,
    configured: Option<&str>,
    same: bool,
    ssh_transport: bool,
    pick: Option<&(String, BuildSource)>,
) {
    let hub_build = hub.build.as_deref().unwrap_or("-");
    let install = format!("`twr box install {name}`");
    // A hub that did not say which build it runs: the box's configured or newest build runs
    // the checks, and the report says why.
    if let Some(why) = &hub.unknown {
        if let Some((id, source)) = pick {
            r.add(
                "build",
                Mark::Warn,
                format!(
                    "the running hub did not say which build it runs ({why}); the checks below \
                     run {} {id}",
                    source.said()
                ),
                None,
            );
            return;
        }
    }
    match pick {
        Some((id, BuildSource::Hub)) => r.add(
            "build",
            Mark::Ok,
            format!("the hub's build {id} is on the box; the checks below run it"),
            None,
        ),
        Some((id, BuildSource::Configured)) if !same => r.add(
            "build",
            Mark::Ok,
            format!("the box's configured build {id} is there; the checks below run it"),
            None,
        ),
        Some((id, source)) => {
            let mark = if ssh_transport { Mark::Warn } else { Mark::Ok };
            let why = match (same, configured) {
                (true, _) => format!(
                    "the hub's build {hub_build} is not on the box yet (the hub uploads it as \
                     it connects)"
                ),
                (false, Some(c)) => format!("the box's configured build {c} is not on the box"),
                (false, None) => "the box has no configured build".to_string(),
            };
            r.add(
                "build",
                mark,
                format!("{why}; the checks below run {} {id}", source.said()),
                (ssh_transport && !same).then(|| install.clone()),
            );
        }
        None if !ssh_transport => r.add(
            "build",
            Mark::Warn,
            "no twr build in the box's builds directory (a token box runs the twr you \
             installed): the box's agents are not probed over ssh"
                .into(),
            None,
        ),
        None if same => r.add(
            "build",
            Mark::Warn,
            format!(
                "no twr build on the box yet (the hub uploads its own, {hub_build}, as it \
                 connects): the daemon and the agents are not checked"
            ),
            Some(format!("{install} has the hub try now")),
        ),
        None => r.add(
            "build",
            Mark::Fail,
            "no twr build on the box: the hub cannot run its own on another OS or CPU, so the \
             daemon and the agents are not checked"
                .into(),
            Some(install),
        ),
    }
}

/// The box daemon of this hub on box `name`: its status line from `side` (the second ssh's
/// output), against the build the hub launches and its protocol.
#[allow(clippy::too_many_arguments)]
fn daemon_check(
    r: &mut Report,
    name: &str,
    hub: &HubBuild,
    configured: Option<&str>,
    same: bool,
    pick: Option<&(String, BuildSource)>,
    has_key: bool,
    side: Option<&Result<String, String>>,
) {
    let Some((id, _)) = pick else {
        return;
    };
    if !has_key {
        r.add(
            "daemon",
            Mark::Warn,
            "not checked: the hub has no box key yet (it has never connected to an SSH box)".into(),
            None,
        );
        return;
    }
    let out = match side {
        Some(Ok(out)) => out,
        Some(Err(why)) => {
            r.add("daemon", Mark::Warn, format!("not checked: {why}"), None);
            return;
        }
        None => return,
    };
    let (code, lines) = section(out, STATUS_MARK).unwrap_or((None, Vec::new()));
    let line = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty());
    if code != Some(0) {
        r.add(
            "daemon",
            Mark::Warn,
            format!("not checked: {}", box_side_failure(id, "--status", code)),
            Some(format!(
                "`twr box install {name}` installs this twr's build"
            )),
        );
        return;
    }
    if line == Some("no daemon") {
        r.add(
            "daemon",
            Mark::Warn,
            "no box daemon of this hub runs on the box (the hub starts one as it connects)".into(),
            None,
        );
        return;
    }
    let Some(st) = line.and_then(twr_runner::daemon::Status::parse) else {
        r.add(
            "daemon",
            Mark::Warn,
            format!("build {id} gave no status line twr understands"),
            None,
        );
        return;
    };
    let launches = if same {
        hub.build.as_deref()
    } else {
        configured
    };
    // The running hub's protocol (no hub runs: this twr's, which a hub started now speaks).
    let whose = if hub.info.is_some() {
        "the hub"
    } else {
        "this twr (no hub runs)"
    };
    let Some(protocol) = hub.protocol else {
        r.add(
            "daemon",
            Mark::Warn,
            format!(
                "a box daemon runs: build {}, runner protocol {}; {}, so whether they match is \
                 unknown",
                st.build_label(),
                st.protocol,
                if hub.info.is_some() {
                    "the running hub doesn't say which protocol it speaks (a twr older than \
                     this one)"
                } else {
                    "twr couldn't tell whether a hub runs, nor which protocol it speaks"
                }
            ),
            Some(
                "restart the hub with this twr (`twr hub stop`, then `twr`), then `twr box \
                 doctor` again"
                    .into(),
            ),
        );
        return;
    };
    if st.protocol != protocol {
        let fix = if same {
            "the hub replaces it as it connects: start the hub (`twr`), or `twr box install \
             <box>` has a running one try now"
                .replace("<box>", name)
        } else {
            format!("`twr box install {name}` installs this hub's build there")
        };
        r.add(
            "daemon",
            if same { Mark::Warn } else { Mark::Fail },
            format!(
                "the box daemon (build {}) speaks runner protocol {}, {whose} {protocol}",
                st.build_label(),
                st.protocol
            ),
            Some(fix),
        );
        return;
    }
    match launches {
        Some(l) if st.build.as_deref() != Some(l) => r.add(
            "daemon",
            Mark::Warn,
            format!(
                "a box daemon runs build {}, not build {l}, which the hub launches: the hub \
                 switches it once it has been quiet for 2 minutes",
                st.build_label()
            ),
            None,
        ),
        _ => r.add(
            "daemon",
            Mark::Ok,
            format!(
                "a box daemon runs: build {}, runner protocol {}",
                st.build_label(),
                st.protocol
            ),
            None,
        ),
    }
}

/// The start of the names of the transient units the relay runs the box daemon of hub key
/// `key` (64 hex digits) and box `name` as: `twr-box-<hash>-` ([`twr_runner::relay::socket_hash`],
/// as the relay names them). `None` for a key that is not one.
fn unit_prefix(name: &str, key: &str) -> Option<String> {
    let public = twr_core::sealed::parse_hub_public(key).ok()?;
    Some(format!(
        "twr-box-{}-",
        twr_runner::relay::socket_hash(&twr_core::sealed::hub_id(&public), name)
    ))
}

/// How the box daemon of this hub runs, as the second ssh saw it ([`box_side_script`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonLaunch {
    /// No daemon answered, or it was not asked.
    NoDaemon,
    /// A daemon answers, and its systemd user service (this unit) is active.
    Service(String),
    /// A daemon answers, outside any active unit of its own: started in a new session (no
    /// lingering, or `systemd-run` failed).
    Session,
    /// A daemon answers; whether it runs in a unit is unknown (`systemctl` failed or did not
    /// answer).
    Unknown,
}

/// [`DaemonLaunch`] from `side`, the second ssh's output, with `prefix` the daemon's unit
/// names' ([`unit_prefix`]).
fn daemon_launch(side: Option<&Result<String, String>>, prefix: Option<&str>) -> DaemonLaunch {
    let Some(Ok(out)) = side else {
        return DaemonLaunch::NoDaemon;
    };
    let answers = match section(out, STATUS_MARK) {
        Some((Some(0), lines)) => lines
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .and_then(twr_runner::daemon::Status::parse)
            .is_some(),
        _ => false,
    };
    if !answers {
        return DaemonLaunch::NoDaemon;
    }
    let Some(prefix) = prefix else {
        return DaemonLaunch::Unknown;
    };
    match section(out, UNITS_MARK) {
        Some((Some(0), lines)) => {
            let unit = lines.iter().find_map(|l| {
                let unit = l.split_whitespace().next()?;
                (unit.starts_with(prefix) && unit.ends_with(".service")).then_some(unit)
            });
            match unit {
                Some(u) => DaemonLaunch::Service(u.chars().take(ssh_box::NOTE_MAX_BYTES).collect()),
                None => DaemonLaunch::Session,
            }
        }
        _ => DaemonLaunch::Unknown,
    }
}

/// logind on a Linux box: whether the box daemon outlives the ssh session that started it.
/// Only a confirmed answer says it does: the daemon (`launch`) running as a systemd user
/// service of a user who lingers (`Linger=yes`), or logind keeping a user's processes
/// (`KillUserProcesses` read as `b false`). Lingering alone is no proof: the relay starts the
/// daemon in a new session when `systemd-run` fails. A query that is missing or failed is
/// unknown, and warned about; a box without `loginctl` at all has no logind to end anything.
/// `stop` is how to stop the daemon, when doctor can say.
fn logind_check(
    r: &mut Report,
    probe: &ssh_box::Probe,
    ins: &Inspected,
    launch: &DaemonLaunch,
    stop: Option<&str>,
) {
    if probe.os != "Linux" {
        return;
    }
    let lingers = ins.linger.as_deref() == Some("Linger=yes");
    let kills = match ins.kill.as_deref() {
        Some("b true") => Some(true),
        Some("b false") => Some(false),
        _ => None,
    };
    let no_loginctl = ins.linger.as_deref() == Some("none");
    let ends = if kills == Some(true) {
        "logind ends your processes with your last session (KillUserProcesses=yes), so"
    } else {
        "couldn't read logind's KillUserProcesses; if it is on,"
    };
    let restart = match stop {
        Some(stop) => format!("then stop the box daemon with `{stop}`"),
        None => "then stop the box daemon (`twr box remove` prints how)".into(),
    };
    match (lingers, kills, launch) {
        (true, _, DaemonLaunch::Service(unit)) => r.add(
            "logind",
            Mark::Ok,
            format!(
                "you linger on the box, and the box daemon runs as the systemd user service {unit}"
            ),
            None,
        ),
        (false, _, DaemonLaunch::Service(unit)) => r.add(
            "logind",
            Mark::Warn,
            format!(
                "the box daemon runs as the systemd user service {unit}, but you don't linger: \
                 your service manager, and the daemon with it, stop with your last session"
            ),
            Some("run `loginctl enable-linger` on the box".into()),
        ),
        (true, Some(false), _) => r.add(
            "logind",
            Mark::Ok,
            "you linger, and logind keeps your processes after your last session: the box \
             daemon runs on whether or not it runs as a systemd user service"
                .into(),
            None,
        ),
        (true, _, DaemonLaunch::Session) => r.add(
            "logind",
            Mark::Warn,
            format!(
                "you linger, but the box daemon runs outside a systemd user service \
                 (`systemd-run` failed when the hub started it): {ends} the box daemon and its \
                 agents end when the hub's ssh does"
            ),
            Some(format!(
                "check that `systemd-run --user --expand-environment=no true` works on the box \
                 (systemd 254 or newer), {restart}: the hub starts it again as a service as it \
                 connects"
            )),
        ),
        (true, _, DaemonLaunch::Unknown) => r.add(
            "logind",
            Mark::Warn,
            format!(
                "you linger, but couldn't tell whether the box daemon runs as a systemd user \
                 service (`systemctl --user list-units` failed): {ends} a daemon outside one \
                 ends when the hub's ssh does"
            ),
            None,
        ),
        (true, _, DaemonLaunch::NoDaemon) => r.add(
            "logind",
            Mark::Warn,
            "you linger, so the hub starts the box daemon as a systemd user service, but no \
             daemon of this hub runs to confirm it"
                .into(),
            Some("`twr box doctor` again once the hub has connected to the box".into()),
        ),
        (false, Some(true), _) => r.add(
            "logind",
            Mark::Warn,
            "logind ends your processes with your last session (KillUserProcesses=yes) and you \
             don't linger: the box daemon and its agents end when the hub's ssh does"
                .into(),
            Some("run `loginctl enable-linger` on the box".into()),
        ),
        (false, Some(false), _) => r.add(
            "logind",
            Mark::Ok,
            "you don't linger, and logind keeps your processes after your last session: the \
             box daemon runs in a session of its own"
                .into(),
            Some("`loginctl enable-linger` on the box runs it as a systemd user service".into()),
        ),
        (false, None, _) if no_loginctl => r.add(
            "logind",
            Mark::Ok,
            "no loginctl on the box (no logind to end your processes): the box daemon runs in \
             a session of its own"
                .into(),
            None,
        ),
        (false, None, _) => r.add(
            "logind",
            Mark::Warn,
            "couldn't read logind's KillUserProcesses; if it is on, the daemon ends with the ssh \
             session unless you `loginctl enable-linger`"
                .into(),
            Some("run `loginctl enable-linger` on the box".into()),
        ),
    }
}

/// Whether a Mac has a GUI session (someone logged in at its screen) by `gui`, the exit status
/// of `launchctl print gui/<uid>` ([`Inspected::gui`]), as the relay reads it: 0 is one, 113
/// (no such domain) none; anything else is not known (`None`: not asked, a `launchctl` that
/// failed otherwise or isn't there, 127, or one cut short by its bound, GNU `timeout`'s 124 or
/// a signal).
fn gui_session(gui: Option<&str>) -> Option<bool> {
    match gui?.trim().parse::<i32>().ok()? {
        0 => Some(true),
        113 => Some(false),
        _ => None,
    }
}

/// How box `name`'s daemon was started, as it says (`launch` of `b`, its `hub.info` entry),
/// on a Mac (`mac`) or not: whether that is as wanted (`Some(true)`; `Some(false)`: a Mac's
/// daemon that runs from ssh; `None`: not known), what is said of it, and the fix. Only while
/// the box is connected (`online`): the hub keeps the last daemon's word while the box is
/// offline, and that daemon may be gone.
pub(super) fn launch_verdict(
    name: &str,
    b: &Value,
    mac: bool,
) -> (Option<bool>, String, Option<String>) {
    if b["state"] != "online" {
        return (
            None,
            format!(
                "not known: the box is not connected (it is {})",
                super::str_of(&b["state"])
            ),
            None,
        );
    }
    match b["launch"].as_str() {
        Some(l) if l.starts_with("launchd ") => (
            Some(true),
            format!(
                "the box daemon runs as the launchd job {}, in the GUI session of whoever is \
                 logged in at the Mac's screen",
                &l["launchd ".len()..]
            ),
            None,
        ),
        Some(l) if l.starts_with("systemd ") => (
            Some(true),
            format!(
                "the box daemon runs as the systemd user service {}",
                &l["systemd ".len()..]
            ),
            None,
        ),
        Some("detached") if mac => (
            Some(false),
            "the daemon runs from ssh, so Claude Code can't read its keychain login".into(),
            Some(format!(
                "log in at the Mac's screen (or turn on automatic login), then `twr box restart \
                 {name}`"
            )),
        ),
        Some("detached") => (
            Some(true),
            "the box daemon runs in a session of its own".into(),
            None,
        ),
        Some(other) => (
            None,
            format!("the box daemon says it was started as {other:?}, which twr doesn't know"),
            None,
        ),
        None if b["version"].is_null() => (
            None,
            "not known: no box daemon has reported to this hub yet".into(),
            None,
        ),
        None => (
            None,
            "the box daemon didn't say how it was started (a twr older than this one)".into(),
            None,
        ),
    }
}

/// A Mac's own checks (box `name`): a GUI session (someone logged in at its screen, `ins`), and
/// how the box daemon was started, as it told the hub (`hub.info` `launch`, while the box is
/// connected; not known without a hub, or for a token box, whose runner you start yourself).
fn macos_checks(r: &mut Report, name: &str, ins: &Inspected, hub: &HubBuild, ssh_transport: bool) {
    let restart = if ssh_transport {
        format!("then `twr box restart {name}`")
    } else {
        "then restart the box's `twr runner` from there".into()
    };
    match gui_session(ins.gui.as_deref()) {
        Some(true) => r.add_item(
            "macos",
            Some("GUI session"),
            Mark::Ok,
            if ssh_transport {
                "someone is logged in at the Mac's screen: the box daemon can run in that GUI \
                 session (a launchd job), where Claude Code can read its keychain login"
            } else {
                "someone is logged in at the Mac's screen: a runner you start in that GUI \
                 session (from a terminal there) can read Claude Code's keychain login"
            }
            .into(),
            None,
        ),
        Some(false) => r.add_item(
            "macos",
            Some("GUI session"),
            Mark::Fail,
            format!(
                "no one is logged in at the Mac's screen (`launchctl print gui/<uid>` found no \
                 GUI session): {}",
                if ssh_transport {
                    "the box daemon then runs from ssh, where Claude Code can't read its \
                     keychain login"
                } else {
                    "a runner you start from ssh can't read Claude Code's keychain login"
                }
            ),
            Some(format!(
                "log in at the Mac's screen, or turn on automatic login (System Settings, Users \
                 & Groups) so it logs in after a restart; {restart}"
            )),
        ),
        None => r.add_item(
            "macos",
            Some("GUI session"),
            Mark::Warn,
            format!(
                "couldn't tell whether someone is logged in at the Mac's screen (`launchctl \
                 print gui/<uid>`: {})",
                ins.gui
                    .as_deref()
                    .map_or_else(|| "no answer".to_string(), |c| format!("exit {c}"))
            ),
            None,
        ),
    }
    if !ssh_transport {
        return;
    }
    let Some(b) = hub.box_entry(name) else {
        r.add_item(
            "macos",
            Some("daemon launch"),
            Mark::Warn,
            "how the box daemon was started is not known: no hub runs that has the box".into(),
            None,
        );
        return;
    };
    let (good, said, fix) = launch_verdict(name, b, true);
    let mark = match good {
        Some(true) => Mark::Ok,
        _ => Mark::Warn,
    };
    r.add_item("macos", Some("daemon launch"), mark, said, fix);
}

/// What a Mac's box daemon (a token box's runner) says of its agent CLIs, which doctor takes
/// over ssh's probe for Claude Code ([`daemon_view`]).
#[derive(Debug, Clone, PartialEq)]
enum DaemonView {
    /// Not asked: not a Mac.
    NotAsked,
    /// Not known, and why: the box is not connected (the hub keeps what a daemon it no longer
    /// reaches said), or no hub runs that has it.
    Unknown(String),
    /// What it says: probed again just now (through the hub, `box.agents` with `refresh`), or,
    /// when that failed (why), as it last reported them on its connection.
    Agents(Vec<AgentInfo>, Option<String>),
}

/// [`DaemonView`] of box `name`, by the hub on `socket` (`hub`, its `hub.info`): only while
/// the box is connected (`online`); the agents probed again first (within [`REFRESH_WAIT`],
/// as `twr box login` asks), else as the box last reported them on its connection.
async fn daemon_view(socket: &Path, hub: &HubBuild, name: &str, json: bool) -> DaemonView {
    let Some(b) = hub.box_entry(name) else {
        return DaemonView::Unknown(
            if hub.info.is_some() {
                "the running hub has no such box"
            } else {
                "no hub runs that has the box"
            }
            .into(),
        );
    };
    if b["state"] != "online" {
        return DaemonView::Unknown(format!(
            "the box is not connected (it is {})",
            super::str_of(&b["state"])
        ));
    }
    progress(
        json,
        &format!("asking box {name} through the hub how its agents are..."),
    );
    let asked = async {
        let mut c = connect(socket).await?;
        c.call("box.agents", json!({ "box": name, "refresh": true }))
            .await
    };
    match tokio::time::timeout(REFRESH_WAIT, asked).await {
        Ok(Ok(v)) => DaemonView::Agents(agents_of(&v), None),
        Ok(Err(e)) => DaemonView::Agents(super::box_agents(b), Some(format!("{e:#}"))),
        Err(_) => DaemonView::Agents(
            super::box_agents(b),
            Some(format!(
                "the hub did not answer within {} s",
                REFRESH_WAIT.as_secs()
            )),
        ),
    }
}

/// The login shell's `PATH` and each agent CLI, from the agent probe in `side` (the second
/// ssh's output); without one, the agents as the box last reported them to the hub, when it
/// did. On a Mac (`view` asked), Claude Code as the box daemon (`ssh_transport`; else the
/// box's runner) sees it, when that is known ([`DaemonView`]).
fn agents_checks(
    r: &mut Report,
    name: &str,
    hub: &HubBuild,
    view: &DaemonView,
    pick: Option<&(String, BuildSource)>,
    side: Option<&Result<String, String>>,
    ssh_transport: bool,
) {
    let probed = match (pick, side) {
        (Some((id, _)), Some(Ok(out))) => match section(out, AGENTS_MARK) {
            Some((Some(0), lines)) => parse_agents(&lines)
                .ok_or_else(|| format!("build {id} printed no agent report twr understands")),
            Some((code, _)) => Err(box_side_failure(id, "--agents", code)),
            None => Err(format!("build {id} printed no agent report")),
        },
        (Some(_), Some(Err(why))) => Err(why.clone()),
        _ => Err("no twr build on the box to probe them with".into()),
    };
    let report = match probed {
        Ok(report) => report,
        Err(why) => {
            let known = hub
                .box_entry(name)
                .map(super::box_agents)
                .unwrap_or_default();
            if known.is_empty() {
                r.add(
                    "agent",
                    Mark::Warn,
                    format!("the box's agent CLIs were not probed: {why}"),
                    None,
                );
                return;
            }
            r.add(
                "agent",
                Mark::Warn,
                format!("not probed now ({why}): as the box last reported them to the hub"),
                None,
            );
            for a in &known {
                agent_check(r, name, a);
            }
            r.agents = Some(known);
            return;
        }
    };
    login_shell_check(r, &report.login_path);
    // On a Mac, Claude Code as the box daemon sees it (through the hub, while the box is
    // connected) over the probe's over ssh, found or not: the keychain can answer each
    // differently, and the daemon looks the CLI up on the PATH it started with.
    let who = if ssh_transport {
        "the box daemon"
    } else {
        "the box's runner"
    };
    let (daemon, said): (&[AgentInfo], String) = match view {
        DaemonView::Agents(d, None) => (d, format!(" (as {who} reported it)")),
        DaemonView::Agents(d, Some(why)) => {
            r.add(
                "agent",
                Mark::Warn,
                format!(
                    "the hub could not have box {name} probe its agents now ({why}): Claude \
                     Code below is as {who} last reported it"
                ),
                None,
            );
            (d, format!(" (as {who} last reported it)"))
        }
        _ => (&[], String::new()),
    };
    let mut agents = report.agents;
    for a in &mut agents {
        let claude = a.runtime == twr_adapters::claude_code::NAME;
        if let (true, DaemonView::Unknown(why)) = (claude, view) {
            agent_check_said(
                r,
                name,
                a,
                &format!(
                    " (over ssh, which on a Mac may not read the keychain: how {who} sees it is \
                     not known, {why})"
                ),
            );
            continue;
        }
        let seen = claude
            .then(|| daemon.iter().find(|d| d.runtime == a.runtime))
            .flatten();
        let Some(d) = seen else {
            agent_check(r, name, a);
            continue;
        };
        if !d.found && a.found {
            // Installed since the daemon started (or on a PATH it does not have): a daemon
            // started now finds it.
            let command = agent_name(&a.runtime);
            r.add_item(
                "agent",
                Some(command),
                Mark::Warn,
                format!(
                    "{}{said}, but over ssh it is found ({}): {who} looks it up on the PATH it \
                     started with, so its threads can't run it",
                    agent_said(d),
                    agent_said(a)
                ),
                Some(if ssh_transport {
                    format!("`twr box restart {name}` starts a daemon that finds it")
                } else {
                    "restart the box's `twr runner` (from a shell whose PATH finds it)".into()
                }),
            );
        } else {
            agent_check_said(r, name, d, &said);
        }
        if d.found && a.found && d.login != a.login {
            r.add_item(
                "agent",
                Some(agent_name(&a.runtime)),
                Mark::Warn,
                format!(
                    "over ssh it says {}, {who} {}: on a Mac, Claude Code keeps its login in the \
                     keychain, which {} in the GUI session reads, and ssh only while it is \
                     unlocked; {who}'s view is the one its threads get",
                    login_word(a.login),
                    login_word(d.login),
                    if ssh_transport {
                        "a box daemon running (a launchd job)"
                    } else {
                        "a runner started"
                    }
                ),
                None,
            );
        }
        *a = d.clone();
    }
    r.login_path = Some(report.login_path);
    r.agents = Some(agents);
}

/// A login state in words.
fn login_word(l: AgentLogin) -> &'static str {
    match l {
        AgentLogin::LoggedIn => "logged in",
        AgentLogin::LoggedOut => "logged out",
        AgentLogin::Unknown => "the login is not known",
    }
}

/// How asking the box's login shell for its `PATH` went (`twr runner --agents`).
fn login_shell_check(r: &mut Report, lp: &LoginPathReport) {
    let shell = lp.shell.as_deref().unwrap_or("-");
    match (lp.entries, &lp.error) {
        (Some(n), _) => r.add(
            "login_shell",
            Mark::Ok,
            format!(
                "{shell} gave its PATH ({n} entries): the box's agents are looked up on it, \
                 then on the PATH ssh gives"
            ),
            None,
        ),
        (None, why) => {
            let why = why.as_deref().unwrap_or("it gave none");
            let fix = if why.starts_with(twr_runner::BOX_PROBE_ENV) {
                "TWR_BOX_PROBE=off in ssh's commands on the box turns the login shell's PATH \
                 off: unset it there"
            } else if lp.shell.is_none() {
                "SHELL is not set in ssh's commands on the box: set it, or put the agents on \
                 the PATH ssh gives (or name each with TWR_CLAUDE_BIN, TWR_CODEX_BIN, \
                 TWR_GROK_BIN)"
            } else {
                "twr asks sh, bash, zsh, ksh and fish (csh and tcsh are not asked): put the \
                 agents on the PATH ssh gives, or name each with TWR_CLAUDE_BIN, TWR_CODEX_BIN, \
                 TWR_GROK_BIN"
            };
            r.add(
                "login_shell",
                Mark::Warn,
                format!("the login shell's PATH was not read ({why}): the box's agents are looked up on the PATH ssh gives"),
                Some(fix.into()),
            );
        }
    }
}

/// One agent CLI of box `name`.
fn agent_check(r: &mut Report, name: &str, a: &AgentInfo) {
    agent_check_said(r, name, a, "");
}

/// [`agent_check`], with `from` after what is said of the agent (who said it).
fn agent_check_said(r: &mut Report, name: &str, a: &AgentInfo, from: &str) {
    let command = agent_name(&a.runtime).to_string();
    let (mark, fix) = match (a.found, a.login) {
        (false, _) => (
            Mark::Warn,
            Some(format!(
                "to run {command} threads on box {name}, install {command} there (on the PATH \
                 its login shell gives), then `twr box doctor {name}`"
            )),
        ),
        (true, AgentLogin::LoggedIn) => (Mark::Ok, None),
        (true, AgentLogin::LoggedOut) => (
            Mark::Warn,
            Some(format!("`twr box login {name} {command}`")),
        ),
        // Grok Build has no login status to read.
        (true, AgentLogin::Unknown) if a.runtime == twr_adapters::grok_build::NAME => {
            (Mark::Ok, None)
        }
        (true, AgentLogin::Unknown) => (
            Mark::Warn,
            Some(format!(
                "`twr box login {name} {command}` if it is not logged in"
            )),
        ),
    };
    r.add_item(
        "agent",
        Some(&command),
        mark,
        format!("{}{from}", agent_said(a)),
        fix,
    );
}

/// Free disk in the box's data directory (`df`, `df -Pk`'s line) against `[threads]
/// disk_warn_gb`.
fn disk_check(r: &mut Report, config: &Config, data: &str, df: Option<&str>) {
    let Some(free) = df.and_then(df_free) else {
        r.add(
            "disk",
            Mark::Warn,
            format!("`df -Pk` gave no free space for {data}"),
            None,
        );
        return;
    };
    let warn_gb = config.threads.disk_warn_gb;
    let said = format!(
        "{} free where the box keeps its data ({data})",
        twr_core::git::human_bytes(free)
    );
    if warn_gb > 0 && free < warn_gb.saturating_mul(1 << 30) {
        r.add(
            "disk",
            Mark::Warn,
            format!("{said}: under [threads] disk_warn_gb ({warn_gb} GB)"),
            Some("free some space on the box (`twr trim` trims threads' build output)".into()),
        );
    } else {
        r.add("disk", Mark::Ok, said, None);
    }
}

/// The free bytes of a `df -Pk` line (`<fs> <blocks> <used> <available> <capacity>%
/// <mount>`): the number before the capacity (a file system or mount point may hold spaces).
fn df_free(line: &str) -> Option<u64> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let cap = words.iter().position(|w| w.ends_with('%'))?;
    let kib: u64 = words.get(cap.checked_sub(1)?)?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// Refuses `twr box login` without a terminal on stdin (`stdin`) and stdout (`stdout`): the
/// login asks things there.
fn require_terminal(stdin: bool, stdout: bool, name: &str, agent: &str) -> anyhow::Result<()> {
    if stdin && stdout {
        return Ok(());
    }
    bail!(
        "`twr box login` needs a terminal on stdin and stdout: {agent}'s login on box {name} \
         asks you things there. Run it from your own terminal"
    )
}

/// The ssh arguments of `twr box login`: with a terminal ([`ssh_box::terminal_ssh_args`]),
/// build `id` under `data` runs `twr runner --login <agent>`.
fn login_ssh_args(ssh: &str, data: &str, id: &str, agent: &str) -> anyhow::Result<Vec<String>> {
    let remote = ssh_box::sh_c(&format!(
        "exec {} runner --login {}",
        sh_word(&ssh_box::build_path(data, id)),
        sh_word(agent)
    ));
    ssh_box::terminal_ssh_args(ssh, &remote).map_err(|why| anyhow!("ssh {ssh:?}: {why}"))
}

/// What `twr box login` says on a Mac box about Claude Code's login over ssh.
const KEYCHAIN_NOTE: &str = "On a Mac, Claude Code keeps its login in the login keychain, which \
                             a login over ssh reaches only while it is unlocked: if the login \
                             does not stick, log in at the Mac once";

/// What `twr box login` says before Claude Code's login on Mac box `name` (of `transport`)
/// that no one is logged in at: the box daemon's Claude Code can't read it yet.
fn no_gui_login_note(name: &str, transport: BoxTransport) -> String {
    let restart = match transport {
        BoxTransport::Ssh => format!("`twr box restart {name}`"),
        BoxTransport::Token => "restart the box's `twr runner` there".to_string(),
    };
    format!(
        "No one is logged in at the Mac's screen: a login made now isn't readable by the box \
         daemon's Claude Code until someone logs in there and the daemon is restarted \
         ({restart}), since only a daemon in that GUI session reads the keychain."
    )
}

/// `twr box login` (see [`super::BoxCmd::Login`]).
pub(super) async fn login(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    agent: &str,
) -> anyhow::Result<ExitCode> {
    refuse_agent_action("log agent CLIs in on boxes")?;
    default_workspace_only(paths, "login")?;
    twr_core::runner::validate_box_name(name)?;
    let Some(runtime) = twr_adapters::agents::agent_runtime(agent) else {
        bail!("{agent:?} is not an agent twr logs in: claude, codex or grok");
    };
    let command = agent_name(runtime).to_string();
    require_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        name,
        &command,
    )?;
    let Some(cfg) = config.boxes.get(name) else {
        bail!("there is no box {name} in config.toml or boxes.toml (`twr box list` lists them)");
    };
    let login_args = twr_adapters::agents::login_args(runtime)
        .unwrap_or_default()
        .join(" ");
    let Some(ssh) = cfg.ssh.as_deref().filter(|s| !s.trim().is_empty()) else {
        bail!(
            "box {name} has no ssh destination ([boxes.{name}] ssh): log in on the box itself \
             with `{command} {login_args}`"
        );
    };
    let hub = running_hub(socket).await;

    outln!("probing box {name} over ssh ({ssh})...");
    let remote = ssh_box::sh_c(&inspect_script(None));
    let probing = ssh_here(ssh, &remote, None);
    let (code, out) = tokio::time::timeout(PROBE_WAIT, probing)
        .await
        .map_err(|_| {
            anyhow!(
                "probing box {name} did not finish within {} s (a question of ssh's left \
                 unanswered, or a box that does not answer)",
                PROBE_WAIT.as_secs()
            )
        })??;
    if code != Some(0) {
        let code = code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
        bail!("ssh to box {name} failed ({code}): see ssh's message above");
    }
    let ins = parse_inspect(&out, 0)?;
    if let Some(why) = ins.probe.tree_refusal(name) {
        bail!("{why}");
    }
    let Some(data) = ins.probe.data.clone() else {
        bail!("box {name} has neither XDG_DATA_HOME nor HOME set to an absolute path");
    };
    let Some((id, source)) = pick_build(
        &ins.probe.has,
        &ins.newest_first,
        hub.build.as_deref(),
        cfg.build.as_deref(),
    ) else {
        bail!(
            "box {name} has no twr build to run the login with: `twr box install {name}` (an \
             SSH box of the hub's OS and CPU gets the hub's as the hub connects), or log in on \
             the box itself with `{command} {login_args}`"
        );
    };
    let mac = ins.probe.os == "Darwin";
    let args = login_ssh_args(ssh, &data, &id, &command)?;
    match &hub.unknown {
        Some(why) => outln!(
            "box {name} runs the login with {} {id} (the running hub did not say which build \
             it runs: {why})",
            source.said()
        ),
        None => outln!("box {name} runs the login with {} {id}", source.said()),
    }
    outln!(
        "this starts {command}'s login on box {name} in this terminal ({command} {login_args}); \
         twr doesn't see what you type"
    );
    if mac && runtime == twr_adapters::claude_code::NAME {
        if gui_session(ins.gui.as_deref()) == Some(false) {
            outln!("{}", no_gui_login_note(name, cfg.transport()));
        }
        outln!("{KEYCHAIN_NOTE}.");
    }
    let status = tokio::process::Command::new(ssh_box::ssh_program())
        .args(&args)
        .status()
        .await
        .map_err(|e| anyhow!("running {}: {e}", ssh_program_name()))?;
    let ended = match status.code() {
        Some(0) => "ended".to_string(),
        Some(c) => format!("ended with exit {c}"),
        None => "ended by a signal".to_string(),
    };
    outln!("{command}'s login on box {name} {ended}; asking whether {command} is logged in...");

    let (from, seen) = state_after_login(socket, &hub, name, ssh, &data, &id, runtime).await;
    let Some((a, daemon)) = seen else {
        outln!("{from} did not say whether {command} is logged in on box {name}");
        return Ok(ExitCode::from(1));
    };
    outln!("{command} on box {name}: {} ({from})", agent_said(&a));
    let keychain = mac && runtime == twr_adapters::claude_code::NAME;
    let verdict = login_verdict(&a);
    match verdict {
        LoginVerdict::Missing if daemon && status.success() => {
            // `--login` found the CLI (on the login shell's PATH as it is now), the daemon,
            // which read that PATH when it started, does not.
            outln!("{}", daemon_missing_note(cfg.transport(), name, &command));
        }
        LoginVerdict::Missing => outln!(
            "{from} doesn't find {command} on box {name} (its login shell's PATH included): \
             install it there, or point twr at it with {} in the box's environment; then `twr \
             box doctor {name}`",
            twr_adapters::resolve::BUILTIN_AGENTS
                .iter()
                .find(|(r, ..)| *r == runtime)
                .map_or("its TWR_*_BIN", |(.., v)| v)
        ),
        LoginVerdict::LoggedOut if status.success() => outln!(
            "the login ended without an error, but {from} says {command} is not logged in{}",
            if keychain {
                format!(". {KEYCHAIN_NOTE}")
            } else {
                String::new()
            }
        ),
        LoginVerdict::LoggedIn if !status.success() => {
            outln!("the login {ended}, but {from} says {command} is logged in")
        }
        _ => {}
    }
    Ok(if verdict.succeeded(status.success()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// What `twr box login` says when its login found `command` on box `name` (of `transport`)
/// but the box's daemon (or runner) does not: it read the login shell's PATH when it started,
/// so restarting it reads the PATH again (`twr box restart` for an SSH box).
fn daemon_missing_note(transport: BoxTransport, name: &str, command: &str) -> String {
    let restart = match transport {
        BoxTransport::Ssh => format!(
            "run `twr box restart {name}` (it stops the daemon and its agents and starts a new \
             one; their next prompts resume them)"
        ),
        BoxTransport::Token => {
            "restart the box's `twr runner` (its agents stop with it)".to_string()
        }
    };
    format!(
        "the login found {command} on box {name}, but the box daemon doesn't: it reads the login \
         shell's PATH once, when it starts, so a PATH entry added since is not in it. To have it \
         read the PATH again, {restart}; then `twr box doctor {name}`"
    )
}

/// What `twr box login` makes of the agent's state after the login ([`login_verdict`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginVerdict {
    /// Its executable was not found.
    Missing,
    /// Found and logged in.
    LoggedIn,
    /// Found and logged out.
    LoggedOut,
    /// Found; whether it is logged in is not known (Grok Build, or a status that failed).
    Unknown,
}

impl LoginVerdict {
    /// Whether `twr box login` succeeds (exit 0), its login having ended with or without an
    /// error (`login_ok`): logged in; or found with an unknown state after a login that
    /// ended without one. Never when the executable was not found.
    fn succeeded(self, login_ok: bool) -> bool {
        match self {
            LoginVerdict::LoggedIn => true,
            LoginVerdict::Unknown => login_ok,
            LoginVerdict::Missing | LoginVerdict::LoggedOut => false,
        }
    }
}

/// What agent `a`, as reported after the login, says of it.
fn login_verdict(a: &AgentInfo) -> LoginVerdict {
    match (a.found, a.login) {
        (false, _) => LoginVerdict::Missing,
        (true, AgentLogin::LoggedIn) => LoginVerdict::LoggedIn,
        (true, AgentLogin::LoggedOut) => LoginVerdict::LoggedOut,
        (true, AgentLogin::Unknown) => LoginVerdict::Unknown,
    }
}

/// Agent `runtime` on box `name` after a login, and who said so: the box daemon through the
/// hub (`box.agents` with `refresh`) when the hub has the box, else (or when that fails) the
/// agent probe over ssh, with build `id` under `data`. With the agent, whether the box
/// daemon said it (`true`) or the probe over ssh.
async fn state_after_login(
    socket: &Path,
    hub: &HubBuild,
    name: &str,
    ssh: &str,
    data: &str,
    id: &str,
    runtime: &str,
) -> (String, Option<(AgentInfo, bool)>) {
    let find = |agents: Vec<AgentInfo>| agents.into_iter().find(|a| a.runtime == runtime);
    if hub.box_entry(name).is_some() {
        let asked = async {
            let mut c = connect(socket).await?;
            c.call("box.agents", json!({ "box": name, "refresh": true }))
                .await
        };
        match tokio::time::timeout(REFRESH_WAIT, asked).await {
            Ok(Ok(v)) => {
                return (
                    "the box daemon, through the hub".into(),
                    find(agents_of(&v)).map(|a| (a, true)),
                )
            }
            Ok(Err(e)) => errln!("twr: the hub could not have box {name} probe its agents ({e:#}): asking the box over ssh"),
            Err(_) => errln!(
                "twr: the hub did not answer within {} s: asking the box over ssh",
                REFRESH_WAIT.as_secs()
            ),
        }
    }
    let from = "the box, over ssh".to_string();
    match run_box_side(name, ssh, &box_side_script(data, id, None, true)).await {
        Ok(out) => match section(&out, AGENTS_MARK) {
            Some((Some(0), lines)) => (
                from,
                parse_agents(&lines)
                    .and_then(|r| find(r.agents))
                    .map(|a| (a, false)),
            ),
            Some((code, _)) => {
                errln!("twr: {}", box_side_failure(id, "--agents", code));
                (from, None)
            }
            None => (from, None),
        },
        Err(why) => {
            errln!("twr: {why}");
            (from, None)
        }
    }
}

/// The agents of a `box.agents` result `v`, cut as the probe cuts them.
fn agents_of(v: &Value) -> Vec<AgentInfo> {
    let line = serde_json::to_string(&json!({ "agents": v["agents"] })).unwrap_or_default();
    parse_agents(&[line.as_str()])
        .map(|r| r.agents)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::install;

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// A hub that did not say which build it runs has no build here (not this twr's): the
    /// box's configured build, else its newest, runs the checks, and the report says why.
    #[test]
    fn an_unknown_hub_build_is_not_this_twrs() {
        let hub = HubBuild::own(Some("it did not answer within 5 s".into()));
        assert_eq!(hub.build, None);
        let mine = twr_core::build_id().unwrap_or("0123456789abcdef");
        let has = set(&[mine, "1111111111111111", "2222222222222222"]);
        let newest = ["2222222222222222".to_string()];
        // This twr's build is on the box: still not the hub's.
        let pick = pick_build(
            &has,
            &newest,
            hub.build.as_deref(),
            Some("1111111111111111"),
        );
        assert_eq!(
            pick,
            Some(("1111111111111111".to_string(), BuildSource::Configured))
        );
        let pick = pick_build(&has, &newest, hub.build.as_deref(), None);
        assert_eq!(
            pick,
            Some(("2222222222222222".to_string(), BuildSource::Newest))
        );
        let mut r = Report::new("gpu", true);
        build_check(&mut r, "gpu", &hub, None, true, true, pick.as_ref());
        let c = r.checks.pop().unwrap();
        assert_eq!(c.result, Mark::Warn);
        assert_eq!(
            c.message,
            "the running hub did not say which build it runs (it did not answer within 5 s); \
             the checks below run the newest build on the box 2222222222222222"
        );
    }

    /// The daemon's protocol is compared with the running hub's (`hub.info`
    /// `runner_protocol`), not this twr's: a hub that doesn't say leaves it unknown, and only
    /// with no hub running is this twr's protocol used (and said so).
    #[test]
    fn the_daemon_protocol_is_compared_with_the_running_hubs() {
        let id = "0123456789abcdef";
        let mine = twr_core::runner::PROTOCOL_VERSION;
        let side = |protocol: u32| -> Result<String, String> {
            Ok(format!(
                "{STATUS_MARK}\ntwr-box 1 {id} {protocol} fedcba9876543210\n{EXIT_MARK}0\n"
            ))
        };
        let pick = (id.to_string(), BuildSource::Hub);
        let check = |hub: &HubBuild, protocol: u32| {
            let mut r = Report::new("gpu", true);
            daemon_check(
                &mut r,
                "gpu",
                hub,
                None,
                true,
                Some(&pick),
                true,
                Some(&side(protocol)),
            );
            let c = r.checks.pop().unwrap();
            (c.result, c.message)
        };
        let running = |protocol: Option<u32>| {
            let mut info = json!({"version": "0.2.0", "build": id});
            if let Some(p) = protocol {
                info["runner_protocol"] = p.into();
            }
            HubBuild::of_info(info)
        };
        // A running hub of another protocol than this twr: its own counts.
        let (mark, msg) = check(&running(Some(mine + 1)), mine + 1);
        assert_eq!(mark, Mark::Ok, "{msg}");
        let (mark, msg) = check(&running(Some(mine + 1)), mine);
        assert_eq!(mark, Mark::Warn);
        assert!(msg.ends_with(&format!("the hub {}", mine + 1)), "{msg}");
        // One that doesn't say: unknown, whatever the daemon speaks.
        let (mark, msg) = check(&running(None), mine);
        assert_eq!(mark, Mark::Warn);
        assert!(msg.contains("whether they match is unknown"), "{msg}");
        // No hub: this twr's, said so.
        let mut none = HubBuild::own(None);
        none.build = Some(id.into());
        assert_eq!(check(&none, mine).0, Mark::Ok);
        let (_, msg) = check(&none, mine + 1);
        assert!(
            msg.ends_with(&format!("this twr (no hub runs) {mine}")),
            "{msg}"
        );
        // Whether a hub runs is unknown: so is its protocol.
        let (_, msg) = check(&HubBuild::own(Some("no answer".into())), mine);
        assert!(msg.contains("whether they match is unknown"), "{msg}");
    }

    /// `twr box login` succeeds only when the agent is logged in, or found with an unknown
    /// state after a login that ended without an error: never when the reported agent is not
    /// found (a daemon whose PATH predates the agent's install), whatever the login said.
    #[test]
    fn a_login_succeeds_only_with_the_agent_found() {
        let agent = |found: bool, login: AgentLogin| AgentInfo {
            runtime: "grok-build".into(),
            found,
            login,
            ..Default::default()
        };
        use AgentLogin::*;
        for (found, login, login_ok, verdict, ok) in [
            (true, LoggedIn, true, LoginVerdict::LoggedIn, true),
            (true, LoggedIn, false, LoginVerdict::LoggedIn, true),
            (true, LoggedOut, true, LoginVerdict::LoggedOut, false),
            (true, Unknown, true, LoginVerdict::Unknown, true),
            (true, Unknown, false, LoginVerdict::Unknown, false),
            (false, Unknown, true, LoginVerdict::Missing, false),
            (false, Unknown, false, LoginVerdict::Missing, false),
            (false, LoggedIn, true, LoginVerdict::Missing, false),
        ] {
            let v = login_verdict(&agent(found, login));
            assert_eq!(v, verdict, "{found} {login:?} {login_ok}");
            assert_eq!(v.succeeded(login_ok), ok, "{found} {login:?} {login_ok}");
        }
    }

    /// logind: the daemon is said to outlive the ssh session only on a confirmed answer (it
    /// runs as a systemd user service of a user who lingers, or `KillUserProcesses` read as
    /// `b false`); lingering alone is not one (`systemd-run` may have failed, and the daemon
    /// runs in a session); a missing or failed query is a warning, `b true` without lingering
    /// too; a box without `loginctl` has no logind.
    #[test]
    fn logind_survival_is_claimed_only_when_confirmed() {
        use DaemonLaunch::{NoDaemon, Service, Session, Unknown};
        let probe = ssh_box::Probe {
            os: "Linux".into(),
            ..Default::default()
        };
        let check = |linger: Option<&str>, kill: Option<&str>, launch: &DaemonLaunch| {
            let mut r = Report::new("b", true);
            let ins = Inspected {
                linger: linger.map(str::to_string),
                kill: kill.map(str::to_string),
                ..Default::default()
            };
            logind_check(&mut r, &probe, &ins, launch, Some("ssh box stop"));
            let c = r.checks.pop().unwrap();
            (c.result, c.message, c.fix)
        };
        let unit = Service("twr-box-0123456789abcdef-1a2b3c4d.service".into());
        let unknown = "couldn't read logind's KillUserProcesses; if it is on, the daemon ends \
                       with the ssh session unless you `loginctl enable-linger`";
        for (linger, kill, launch, mark) in [
            (Some("Linger=yes"), None, &unit, Mark::Ok),
            (Some("Linger=yes"), Some("b true"), &unit, Mark::Ok),
            (Some("Linger=yes"), Some("b false"), &Session, Mark::Ok),
            (Some("Linger=yes"), Some("b false"), &NoDaemon, Mark::Ok),
            (Some("Linger=yes"), Some("b true"), &Session, Mark::Warn),
            (Some("Linger=yes"), None, &Session, Mark::Warn),
            (Some("Linger=yes"), Some("b true"), &Unknown, Mark::Warn),
            (Some("Linger=yes"), Some("b true"), &NoDaemon, Mark::Warn),
            (Some("Linger=yes"), None, &NoDaemon, Mark::Warn),
            (Some("Linger=no"), Some("b false"), &Session, Mark::Ok),
            (Some("Linger=no"), Some("b false"), &unit, Mark::Warn),
            (Some("Linger=no"), Some("b true"), &Session, Mark::Warn),
            (Some("none"), None, &Session, Mark::Ok),
            (Some("none"), Some("b true"), &Session, Mark::Warn),
        ] {
            assert_eq!(
                check(linger, kill, launch).0,
                mark,
                "{linger:?} {kill:?} {launch:?}"
            );
        }
        // Lingering, but `systemd-run` failed when the relay started the daemon: it runs in a
        // session logind ends, and the fix says how to get it started as a service.
        let (_, said, fix) = check(Some("Linger=yes"), Some("b true"), &Session);
        assert!(
            said.contains("you linger, but the box daemon runs outside a systemd user service"),
            "{said}"
        );
        assert!(said.contains("KillUserProcesses=yes"), "{said}");
        let fix = fix.unwrap();
        assert!(
            fix.contains("systemd-run --user --expand-environment=no"),
            "{fix}"
        );
        assert!(fix.contains("`ssh box stop`"), "{fix}");
        // Unknown: busctl missing (no `kill=`), its query failed (empty), or an answer that
        // is neither; loginctl's own answer failed too.
        for (linger, kill) in [
            (Some("Linger=no"), None),
            (Some("Linger=no"), Some("")),
            (Some("Linger=no"), Some("Failed to get property")),
            (Some(""), None),
            (None, None),
        ] {
            let (mark, said, _) = check(linger, kill, &Session);
            assert_eq!(
                (mark, said),
                (Mark::Warn, unknown.to_string()),
                "{linger:?} {kill:?}"
            );
        }
        // Not Linux: nothing to say.
        let mut r = Report::new("b", true);
        let mac = ssh_box::Probe {
            os: "Darwin".into(),
            ..Default::default()
        };
        logind_check(&mut r, &mac, &Inspected::default(), &NoDaemon, None);
        assert!(r.checks.is_empty());
    }

    /// The daemon's launch from the second ssh: a service only when a daemon answers and an
    /// active unit with its name (the relay's `twr-box-<hash>-`) is listed; a daemon outside
    /// one (none listed, another hub's or box's only, no `systemctl`) runs in a session; a
    /// failed or cut `systemctl` is unknown; no daemon, nothing to say.
    #[test]
    fn the_daemons_launch_is_read_from_its_units() {
        let key = "ab".repeat(32);
        let prefix = unit_prefix("gpu", &key).unwrap();
        let public = twr_core::sealed::parse_hub_public(&key).unwrap();
        assert_eq!(
            prefix,
            format!(
                "twr-box-{}-",
                twr_runner::relay::socket_hash(&twr_core::sealed::hub_id(&public), "gpu")
            )
        );
        assert_ne!(unit_prefix("cpu", &key), Some(prefix.clone()));
        assert_eq!(unit_prefix("gpu", "ab"), None);
        let status =
            format!("{STATUS_MARK}\ntwr-box 1 00000000000000aa 7 0123456789abcdef\n{EXIT_MARK}0\n");
        let with_units = |units: &str| Ok(format!("{status}{UNITS_MARK}\n{units}"));
        let unit = format!("{prefix}1a2b3c4d.service");
        for (out, want) in [
            (
                with_units(&format!("{unit} loaded active running /x/twr runner\n{EXIT_MARK}0\n")),
                DaemonLaunch::Service(unit.clone()),
            ),
            (with_units(&format!("{EXIT_MARK}0\n")), DaemonLaunch::Session),
            (
                with_units(&format!(
                    "twr-box-ffffffffffffffff-1a2b3c4d.service loaded active running x\n{EXIT_MARK}0\n"
                )),
                DaemonLaunch::Session,
            ),
            (
                with_units(&format!("{NO_SYSTEMCTL}\n{EXIT_MARK}0\n")),
                DaemonLaunch::Session,
            ),
            (with_units(&format!("{EXIT_MARK}1\n")), DaemonLaunch::Unknown),
            (with_units(""), DaemonLaunch::Unknown),
            (Ok(status.clone()), DaemonLaunch::Unknown),
            (
                Ok(format!("{STATUS_MARK}\nno daemon\n{EXIT_MARK}0\n{UNITS_MARK}\n{EXIT_MARK}0\n")),
                DaemonLaunch::NoDaemon,
            ),
            (Err("ssh failed".to_string()), DaemonLaunch::NoDaemon),
        ] {
            assert_eq!(daemon_launch(Some(&out), Some(&prefix)), want, "{out:?}");
        }
        assert_eq!(daemon_launch(None, Some(&prefix)), DaemonLaunch::NoDaemon);
    }

    /// R1: the hub's build when the box has it, else the configured one, else the newest the
    /// box has and twr trusts.
    #[test]
    fn the_build_is_the_hubs_else_the_configured_else_the_newest() {
        let (h, c, a, b) = (
            "000000000000000a",
            "000000000000000c",
            "00000000000000aa",
            "00000000000000bb",
        );
        let has = set(&[h, c, a, b]);
        let order = [b.to_string(), a.to_string()];
        assert_eq!(
            pick_build(&has, &order, Some(h), Some(c)),
            Some((h.to_string(), BuildSource::Hub))
        );
        assert_eq!(
            pick_build(&set(&[c, a]), &order, Some(h), Some(c)),
            Some((c.to_string(), BuildSource::Configured))
        );
        // The newest of those trusted: `bb` is newest but not trusted here.
        assert_eq!(
            pick_build(&set(&[a]), &order, Some(h), Some(c)),
            Some((a.to_string(), BuildSource::Newest))
        );
        // No order known: any trusted one.
        assert_eq!(
            pick_build(&set(&[a]), &[], None, None),
            Some((a.to_string(), BuildSource::Newest))
        );
        assert_eq!(pick_build(&set(&[]), &order, Some(h), Some(c)), None);
    }

    /// The first ssh's script runs under `sh` here: the probe, then the builds newest first,
    /// git, df and each project's repository, read back by the parser.
    #[test]
    fn the_inspect_script_runs_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let builds = home.join(".local/share/twr/runner/builds");
        for (i, id) in ["00000000000000aa", "00000000000000bb"].iter().enumerate() {
            let d = builds.join(id);
            std::fs::create_dir_all(&d).unwrap();
            let exe = d.join("twr");
            std::fs::write(&exe, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
            // `bb` newer.
            let t = std::time::SystemTime::now() - Duration::from_secs(100 - 50 * i as u64);
            std::fs::File::open(&d).unwrap().set_modified(t).unwrap();
        }
        for d in [
            home.join(".local"),
            home.join(".local/share"),
            home.join(".local/share/twr"),
            home.join(".local/share/twr/runner"),
            builds.clone(),
        ] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let repo = home.join("repo");
        twr_core::testkit::git::init_repo(&repo);
        let plain = home.join("plain dir");
        std::fs::create_dir_all(&plain).unwrap();
        let repos = vec![
            repo.display().to_string(),
            plain.display().to_string(),
            home.join("nope").display().to_string(),
        ];
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(inspect_script(Some(&repos)))
            .env("HOME", home)
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert!(out.status.success());
        let ins = parse_inspect(&String::from_utf8_lossy(&out.stdout), repos.len()).unwrap();
        // (The temp dir's ancestors may not be safe on every machine: only checked when so.)
        if !ins.probe.tree_unsafe {
            assert_eq!(
                ins.probe.has,
                set(&["00000000000000aa", "00000000000000bb"])
            );
        }
        assert_eq!(ins.newest_first, ["00000000000000bb", "00000000000000aa"]);
        assert!(
            ins.git.as_deref().unwrap().starts_with("git version"),
            "{ins:?}"
        );
        assert!(df_free(ins.df.as_deref().unwrap()).is_some(), "{ins:?}");
        assert_eq!(
            ins.projects,
            [
                Some("git".into()),
                Some("notgit".into()),
                Some("missing".into())
            ]
        );
        // Without repositories (`twr box login`): the probe and the builds only.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(inspect_script(None))
            .env("HOME", home)
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        let ins = parse_inspect(&String::from_utf8_lossy(&out.stdout), 0).unwrap();
        assert_eq!(ins.newest_first.len(), 2);
        assert!(ins.git.is_none());
        assert_eq!(ins.home.as_deref(), home.to_str());
    }

    /// A box-side command that hangs (a `df` of a dead mount) is ended at the bound, with
    /// the box's `timeout` and without one: the script ends with doctor, not after it.
    #[test]
    fn the_box_side_commands_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let slow = dir.path().join("slow");
        install(&slow.join("df"), "#!/bin/sh\nexec sleep 30\n");
        // A box without a `timeout` that works.
        let no_timeout = dir.path().join("no-timeout");
        install(&no_timeout.join("timeout"), "#!/bin/sh\nexit 1\n");
        let path = std::env::var("PATH").unwrap_or_default();
        for extra in [vec![&slow], vec![&no_timeout, &slow]] {
            let mut dirs: Vec<String> = extra.iter().map(|d| d.display().to_string()).collect();
            dirs.push(path.clone());
            let started = std::time::Instant::now();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(inspect_script_bounded(Some(&[]), 1))
                .env("HOME", &home)
                .env("PATH", dirs.join(":"))
                .env_remove("XDG_DATA_HOME")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let took = started.elapsed();
            assert!(out.status.success(), "{extra:?}");
            assert!(took < Duration::from_secs(10), "{extra:?} took {took:?}");
            let ins = parse_inspect(&String::from_utf8_lossy(&out.stdout), 0).unwrap();
            assert_eq!(ins.df.as_deref(), Some(""), "{extra:?}: {ins:?}");
            assert!(
                ins.git
                    .as_deref()
                    .is_some_and(|g| g.starts_with("git version")),
                "{extra:?}: {ins:?}"
            );
        }
    }

    /// Without `timeout` (a Mac), a bounded command that ends early leaves nothing behind: its
    /// watcher's `sleep` ends with the watcher, before `twr_b` returns (each `sleep` the
    /// watchers start writes its pid into a file; none of them runs afterwards), and its exit
    /// status is the command's. Under `sh` and, where there is one, `bash` (a Mac's `/bin/sh`).
    #[test]
    fn a_bounded_command_that_ends_early_leaves_no_watcher_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        let real_sleep = std::env::split_paths(&path)
            .map(|d| d.join("sleep"))
            .find(|p| p.is_file())
            .expect("a sleep on PATH");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let write =
            |name: &str, body: String| install(&bin.join(name), format!("#!/bin/sh\n{body}\n"));
        write("timeout", "exit 1".into());
        write(
            "sleep",
            format!(
                "echo $$ >> \"$TWR_TEST_PIDS\"\nexec {} \"$@\"",
                real_sleep.display()
            ),
        );
        let shells: Vec<&str> = ["sh", "bash"]
            .into_iter()
            .filter(|sh| {
                std::process::Command::new(sh)
                    .args(["-c", "exit 0"])
                    .status()
                    .is_ok_and(|s| s.success())
            })
            .collect();
        for sh in shells {
            let pids = dir.path().join(format!("{sh}.pids"));
            let script = format!(
                "{prelude}\
                 twr_b sh -c 'until [ -s \"$0\" ]; do {real} 0.05; done' \"$TWR_TEST_PIDS\"\n\
                 echo \"first=$?\"\n\
                 twr_b sh -c 'exit 3'\n\
                 echo \"second=$?\"\n\
                 for p in $(cat \"$TWR_TEST_PIDS\"); do\n\
                 \x20 if kill -0 \"$p\" 2>/dev/null; then echo \"left=$p\"; kill \"$p\"; fi\n\
                 done\n",
                prelude = bounded_prelude(30),
                real = real_sleep.display(),
            );
            let started = std::time::Instant::now();
            let out = std::process::Command::new(sh)
                .arg("-c")
                .arg(&script)
                .env("PATH", format!("{}:{path}", bin.display()))
                .env("TWR_TEST_PIDS", &pids)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(started.elapsed() < Duration::from_secs(20), "{sh}: {said}");
            assert_eq!(said, "first=0\nsecond=3\n", "{sh}");
            let started = std::fs::read_to_string(&pids).unwrap();
            assert!(
                !started.trim().is_empty(),
                "{sh}: no watcher's sleep started"
            );
        }
    }

    /// The second ssh's `twr runner` commands that hang on the box are ended at their bound,
    /// with the box's `timeout` and without one: the script ends with doctor, not after it,
    /// and each section says it did not end well.
    #[test]
    fn the_box_side_runner_commands_are_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let id = "00000000000000aa";
        let exe = std::path::PathBuf::from(ssh_box::build_path(data.to_str().unwrap(), id));
        install(&exe, "#!/bin/sh\nexec sleep 30\n");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let no_timeout = dir.path().join("no-timeout");
        std::fs::create_dir(&no_timeout).unwrap();
        install(&no_timeout.join("timeout"), "#!/bin/sh\nexit 1\n");
        let path = std::env::var("PATH").unwrap_or_default();
        let script =
            box_side_script_bounded(data.to_str().unwrap(), id, Some(("gpu", "ab")), true, 1, 1);
        for extra in [None, Some(&no_timeout)] {
            let dirs = match extra {
                Some(d) => format!("{}:{path}", d.display()),
                None => path.clone(),
            };
            let started = std::time::Instant::now();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .env("PATH", dirs)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let took = started.elapsed();
            assert!(out.status.success(), "{extra:?}");
            assert!(took < Duration::from_secs(10), "{extra:?} took {took:?}");
            let out = String::from_utf8_lossy(&out.stdout);
            for mark in [STATUS_MARK, AGENTS_MARK] {
                let (code, lines) = section(&out, mark).unwrap();
                assert!(code.is_some_and(|c| c != 0), "{extra:?} {mark}: {code:?}");
                assert!(lines.is_empty(), "{extra:?} {mark}: {lines:?}");
            }
        }
    }

    /// Lines a login script prints, or the box makes up before the mark, are not read; values
    /// are cut.
    #[test]
    fn the_inspect_output_is_read_after_its_marks_only() {
        let out = format!(
            "git=evil\ntwr-probe 1\nos=Linux\narch=x86_64\ndata=/h/.local/share\n\
             {DOCTOR_MARK}\norder=00000000000000aa\norder=../../x\ngit={}\nproject.0=git\n\
             project.7=git\ndf=/dev/sda1 100 50 2048 3% /\n",
            "v".repeat(5000)
        );
        let ins = parse_inspect(&out, 1).unwrap();
        assert_eq!(ins.newest_first, ["00000000000000aa"]);
        assert_eq!(ins.home, None);
        let ins = parse_inspect(
            &format!("{out}home=relative\nhome=/h/me\nhome=/h/other\n"),
            1,
        )
        .unwrap();
        assert_eq!(ins.home.as_deref(), Some("/h/me"));
        assert_eq!(ins.git.as_deref().unwrap().len(), ssh_box::NOTE_MAX_BYTES);
        assert_eq!(ins.projects, [Some("git".into())]);
        assert_eq!(df_free(ins.df.as_deref().unwrap()), Some(2048 * 1024));
        assert_eq!(df_free("/dev/x y 10 5 7 50% /mnt/a b"), Some(7 * 1024));
        assert_eq!(df_free("garbage"), None);
        assert!(parse_inspect("nothing", 0).is_err());
    }

    /// The second ssh's script, run here with a fake build: its sections and exit statuses,
    /// and every variable part quoted.
    #[test]
    fn the_box_side_script_runs_each_command_in_its_section() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("it's data");
        let id = "00000000000000aa";
        let exe = std::path::PathBuf::from(ssh_box::build_path(data.to_str().unwrap(), id));
        install(
            &exe,
            "#!/bin/sh\ncase \"$*\" in\n\
             *--status) echo 'twr-box 1 00000000000000aa 7 0123456789abcdef' ;;\n\
             *--agents) echo '{\"login_path\":{\"error\":\"SHELL is not set\"},\"agents\":[{\"runtime\":\"codex\",\"found\":true,\"login\":\"logged_out\"}]}'; echo oops >&2 ;;\n\
             esac\n",
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = box_side_script(data.to_str().unwrap(), id, Some(("gpu", "ab")), true);
        // No hub key, no unit to look for.
        assert!(!script.contains(UNITS_MARK));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        let out = String::from_utf8(out.stdout).unwrap();
        let (code, lines) = section(&out, STATUS_MARK).unwrap();
        assert_eq!(code, Some(0));
        let st = twr_runner::daemon::Status::parse(lines[0]).unwrap();
        assert_eq!((st.build.as_deref(), st.protocol), (Some(id), 7));
        let (code, lines) = section(&out, AGENTS_MARK).unwrap();
        assert_eq!(code, Some(0));
        let report = parse_agents(&lines).unwrap();
        assert_eq!(report.agents[0].login, AgentLogin::LoggedOut);
        assert_eq!(report.login_path.error.as_deref(), Some("SHELL is not set"));
        // Agents only (`twr box login`): no status section.
        let only = box_side_script(data.to_str().unwrap(), id, None, true);
        assert!(!only.contains(STATUS_MARK) && only.contains("--agents"));
        // An older build: clap's usage error.
        assert!(box_side_failure(id, "--agents", Some(2)).contains("does not know"));
        assert!(section("nothing", AGENTS_MARK).is_none());
        assert_eq!(
            section(&format!("{AGENTS_MARK}\n{{\n"), AGENTS_MARK),
            Some((None, vec!["{"]))
        );
    }

    /// The box daemon's unit, looked for on the box (a fake `systemctl` here) as the relay
    /// names it. Lingering with the daemon in its service is fine; lingering with no unit of
    /// it (`systemd-run` failed, the relay started it in a session) is a warning with the fix
    /// while logind ends the session's processes; a `systemctl` that fails is unknown.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lingering_box_whose_daemon_runs_outside_its_service_is_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let id = "00000000000000aa";
        let exe = std::path::PathBuf::from(ssh_box::build_path(data.to_str().unwrap(), id));
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let mode = dir.path().join("mode");
        let args = dir.path().join("args");
        for (file, body) in [
            (
                exe.clone(),
                "#!/bin/sh\necho 'twr-box 1 00000000000000aa 7 0123456789abcdef'\n".to_string(),
            ),
            (
                bin.join("systemctl"),
                // `$7` is the pattern: a unit of that name.
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\ncase $(cat '{mode}') in\n\
                     unit) echo \"${{7%\\*}}1a2b3c4d.service loaded active running twr\" ;;\n\
                     fail) echo 'Failed to connect to bus' >&2; exit 1 ;;\n\
                     esac\n",
                    args = args.display(),
                    mode = mode.display()
                ),
            ),
        ] {
            install(&file, body);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let key = "cd".repeat(32);
        let prefix = unit_prefix("gpu", &key).unwrap();
        let script = box_side_script(data.to_str().unwrap(), id, Some(("gpu", &key)), false);
        let probe = ssh_box::Probe {
            os: "Linux".into(),
            ..Default::default()
        };
        let lingering = Inspected {
            linger: Some("Linger=yes".into()),
            kill: Some("b true".into()),
            ..Default::default()
        };
        let run = |m: &str| {
            std::fs::write(&mode, m).unwrap();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap();
            let side = Ok(String::from_utf8(out.stdout).unwrap());
            let launch = daemon_launch(Some(&side), Some(&prefix));
            let mut r = Report::new("gpu", true);
            logind_check(&mut r, &probe, &lingering, &launch, None);
            let c = r.checks.pop().unwrap();
            (launch, c.result, c.message, c.fix)
        };

        let (launch, mark, said, _) = run("unit");
        assert_eq!(
            launch,
            DaemonLaunch::Service(format!("{prefix}1a2b3c4d.service"))
        );
        assert_eq!(mark, Mark::Ok, "{said}");
        assert_eq!(
            std::fs::read_to_string(&args).unwrap(),
            format!(
                "--user\nlist-units\n--type=service\n--state=active\n--no-legend\n--plain\n{prefix}*\n"
            )
        );

        // `systemd-run` failed: the daemon answers, with no unit of its own.
        let (launch, mark, said, fix) = run("none");
        assert_eq!(launch, DaemonLaunch::Session);
        assert_eq!(mark, Mark::Warn);
        assert!(
            said.contains("you linger, but the box daemon runs outside a systemd user service"),
            "{said}"
        );
        assert!(fix.unwrap().contains("systemd-run --user"));

        let (launch, mark, ..) = run("fail");
        assert_eq!((launch, mark), (DaemonLaunch::Unknown, Mark::Warn));
    }

    /// What the box reports is cut and cleaned before it is shown.
    #[test]
    fn agents_from_the_box_are_cut_and_cleaned() {
        let long = "p".repeat(5000);
        let line = json!({"agents": [{"runtime": "claude-code", "found": true,
            "path": long, "version": "1.0\u{1b}[2J", "login": "logged_in",
            "method": "me@example.com", "note": "signed in as me@example.com",
            "email": "me@example.com"},
            {"runtime": "rm -rf ~", "found": true, "login": "logged_in"},
            {"runtime": "codex", "found": true, "login": "logged_in", "method": "claude.ai"}]})
        .to_string();
        let r = parse_agents(&[line.as_str()]).unwrap();
        let a = &r.agents[0];
        assert_eq!(a.path.as_deref().unwrap().len(), 1024);
        assert_eq!(a.version.as_deref(), Some("1.0[2J"));
        assert_eq!(a.method.as_deref(), Some("other"));
        assert_eq!(a.note.as_deref(), Some(twr_adapters::agents::OTHER_NOTE));
        // Only the built-in runtimes, and a method only for Claude Code.
        assert_eq!(r.agents.len(), 2);
        assert_eq!(
            (r.agents[1].runtime.as_str(), &r.agents[1].method),
            ("codex", &None)
        );
        let back = serde_json::to_string(&r).unwrap();
        assert!(!back.contains("example.com"), "{back}");
        let v = json!({"box": "gpu", "agents": [{"runtime": "codex", "found": false,
            "login": "unknown"}]});
        assert_eq!(agents_of(&v)[0].runtime, "codex");
        assert!(agents_of(&json!({})).is_empty());
    }

    /// `twr box login`'s ssh: a terminal, the forced options, the build's `--login`.
    #[test]
    fn the_login_runs_the_builds_login_over_ssh_with_a_terminal() {
        let args = login_ssh_args(
            "-p 2222 me@mac",
            "/Users/me/.local/share",
            "00000000000000aa",
            "claude",
        )
        .unwrap();
        let remote = ssh_box::sh_c(
            "exec /Users/me/.local/share/twr/runner/builds/00000000000000aa/twr runner --login \
             claude",
        );
        assert_eq!(
            args,
            ssh_box::terminal_ssh_args("-p 2222 me@mac", &remote).unwrap()
        );
        assert_eq!(args[0], "-t");
        assert!(
            !args.iter().any(|a| a == "-T" || a == "BatchMode=yes"),
            "{args:?}"
        );
        for forced in [
            "ClearAllForwardings=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
            "PermitLocalCommand=no",
            "Tunnel=no",
        ] {
            assert!(args.iter().any(|a| a == forced), "{forced}: {args:?}");
        }
        let at = args.iter().position(|a| a == "me@mac").unwrap();
        assert_eq!(args[at + 1..at + 5], ["-a", "-x", "-k", "--"]);
        assert_eq!(args.last().unwrap(), &remote);
        // The box's words that would forward something are refused here too.
        assert!(login_ssh_args("-A me@mac", "/d", "00000000000000aa", "claude").is_err());
        assert!(login_ssh_args("me@mac -t", "/d", "00000000000000aa", "claude").is_err());
    }

    #[test]
    fn the_login_needs_a_terminal_on_both_ends() {
        assert!(require_terminal(true, true, "gpu", "claude").is_ok());
        for (i, o) in [(false, true), (true, false), (false, false)] {
            let e = require_terminal(i, o, "gpu", "claude").unwrap_err();
            assert!(format!("{e}").contains("needs a terminal"), "{e}");
        }
    }

    /// The checks of a box's agents: logged in is fine, logged out says how to log in, a
    /// missing CLI how to install it; Grok's unknown login is no warning.
    #[test]
    fn agent_checks_say_how_to_fix() {
        let mut r = Report::new("gpu", true);
        let agent = |runtime: &str, found: bool, login: AgentLogin| AgentInfo {
            runtime: runtime.into(),
            found,
            login,
            ..AgentInfo::default()
        };
        agent_check(
            &mut r,
            "gpu",
            &agent("claude-code", true, AgentLogin::LoggedIn),
        );
        agent_check(&mut r, "gpu", &agent("codex", true, AgentLogin::LoggedOut));
        agent_check(
            &mut r,
            "gpu",
            &agent("grok-build", true, AgentLogin::Unknown),
        );
        agent_check(
            &mut r,
            "gpu",
            &agent("grok-build", false, AgentLogin::Unknown),
        );
        let marks: Vec<Mark> = r.checks.iter().map(|c| c.result).collect();
        assert_eq!(marks, [Mark::Ok, Mark::Warn, Mark::Ok, Mark::Warn]);
        assert_eq!(
            r.checks[1].fix.as_deref(),
            Some("`twr box login gpu codex`")
        );
        assert_eq!(r.checks[1].item.as_deref(), Some("codex"));
        assert!(r.checks[3].fix.as_deref().unwrap().contains("install grok"));
        assert_eq!(r.failed(), 0);
        let json = serde_json::to_value(&r.checks[1]).unwrap();
        assert_eq!(
            json,
            json!({"check": "agent", "item": "codex", "result": "warn",
                "message": "logged out", "fix": "`twr box login gpu codex`"})
        );
    }

    /// A box's projects: its own, and the hub's projects that name it.
    #[test]
    fn the_projects_of_a_box_are_its_own_and_those_that_name_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\n\
             [boxes.gpu.projects.api]\nrepo = \"/srv/api\"\n\
             [projects.api]\nrepo = \"/home/me/api\"\nbox = \"gpu\"\n\
             [projects.web]\nrepo = \"/home/me/web\"\nbox = \"gpu\"\n\
             [projects.cli]\nrepo = \"/home/me/cli\"\n",
        )
        .unwrap();
        let c = Config::load(&file).unwrap();
        assert_eq!(
            box_projects(&c, "gpu"),
            [
                ("api".to_string(), "/srv/api".to_string()),
                ("web".to_string(), "/home/me/web".to_string())
            ]
        );
        assert!(box_projects(&c, "other").is_empty());
    }

    /// A Mac's GUI session from `launchctl print gui/<uid>`'s exit status, as the relay reads
    /// it: 0 is one, 113 (no such domain) none; any other failure (a `launchctl` that failed
    /// otherwise or isn't there), a command cut short by its bound, or none asked: not known.
    #[test]
    fn a_macs_gui_session_is_read_from_launchctl() {
        assert_eq!(gui_session(Some("0")), Some(true));
        assert_eq!(gui_session(Some("113")), Some(false));
        for unknown in [
            Some("1"),
            Some("5"),
            Some("127"),
            Some("124"),
            Some("137"),
            Some(""),
            Some("x"),
            None,
        ] {
            assert_eq!(gui_session(unknown), None, "{unknown:?}");
        }
        // The script asks only on a Mac: a `uname` that says Darwin, a `launchctl` that fails.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        install(
            &bin.join("uname"),
            "#!/bin/sh\ncase $1 in -s) echo Darwin ;; *) echo arm64 ;; esac\n",
        );
        let run = |path: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(inspect_script(None))
                .env("HOME", dir.path())
                .env("PATH", path)
                .env_remove("XDG_DATA_HOME")
                .output()
                .unwrap();
            parse_inspect(&String::from_utf8_lossy(&out.stdout), 0).unwrap()
        };
        let path = std::env::var("PATH").unwrap();
        let mac = run(&format!("{}:{path}", bin.display()));
        assert_eq!(mac.probe.os, "Darwin");
        // (`/bin/launchctl` is not there off a Mac: the shell's 127.)
        if !Path::new("/bin/launchctl").exists() {
            assert_eq!(mac.gui.as_deref(), Some("127"));
            assert_eq!(gui_session(mac.gui.as_deref()), None);
        }
        if cfg!(target_os = "linux") {
            assert_eq!(run(&path).gui, None);
        }
    }

    /// How the daemon was started, as doctor and `twr box restart` say it: a Mac's daemon run
    /// from ssh is not as wanted, and the fix restarts it. Only while the box is connected
    /// (what the hub keeps from a daemon it no longer reaches is not known to be so); a box
    /// connected without a word of it: a daemon older than this twr, or none reported yet.
    #[test]
    fn the_daemons_launch_is_judged() {
        let entry = |launch: Option<&str>| json!({"name": "mac", "state": "online", "version": "0.2.0", "launch": launch});
        let (ok, said, fix) = launch_verdict(
            "mac",
            &entry(Some("launchd tools.twr.box.0123456789abcdef.0a1b2c3d")),
            true,
        );
        assert_eq!(ok, Some(true));
        assert!(said.contains("launchd job tools.twr.box.0123456789abcdef.0a1b2c3d"));
        assert!(fix.is_none());
        let (ok, said, fix) = launch_verdict("mac", &entry(Some("detached")), true);
        assert_eq!(ok, Some(false));
        assert_eq!(
            said,
            "the daemon runs from ssh, so Claude Code can't read its keychain login"
        );
        assert!(fix.unwrap().ends_with("`twr box restart mac`"));
        assert_eq!(
            launch_verdict("gpu", &entry(Some("detached")), false).0,
            Some(true)
        );
        let (ok, said, _) =
            launch_verdict("gpu", &entry(Some("systemd twr-box-ab-cd.service")), false);
        assert_eq!(ok, Some(true));
        assert!(said.contains("systemd user service twr-box-ab-cd.service"));
        let (ok, said, _) = launch_verdict("gpu", &entry(None), true);
        assert_eq!(ok, None);
        assert!(said.contains("a twr older than this one"), "{said}");
        let mut never = entry(None);
        never["version"] = Value::Null;
        let (ok, said, _) = launch_verdict("gpu", &never, true);
        assert_eq!(ok, None);
        assert_eq!(
            said,
            "not known: no box daemon has reported to this hub yet"
        );
        // Offline (the hub keeps the last word): not known, whatever it was.
        let mut gone = entry(Some("launchd tools.twr.box.0123456789abcdef.0a1b2c3d"));
        gone["state"] = json!("offline");
        let (ok, said, fix) = launch_verdict("mac", &gone, true);
        assert_eq!(ok, None);
        assert_eq!(said, "not known: the box is not connected (it is offline)");
        assert!(fix.is_none());
    }

    /// doctor's macOS group: the GUI session, then the daemon's launch from the hub's view
    /// (unknown without a hub); a Mac with no one at its screen fails.
    #[test]
    fn the_macos_group_says_what_a_mac_needs() {
        let mut r = Report::new("mac", true);
        let ins = Inspected {
            gui: Some("0".into()),
            ..Inspected::default()
        };
        let hub = HubBuild::of_info(json!({"version": "0.2.0", "boxes": [
            {"name": "mac", "state": "online", "launch": "detached", "os": "Darwin"}
        ]}));
        macos_checks(&mut r, "mac", &ins, &hub, true);
        let got: Vec<(String, Mark)> = r.checks.iter().map(|c| (c.label(), c.result)).collect();
        assert_eq!(
            got,
            [
                ("macOS GUI session".to_string(), Mark::Ok),
                ("macOS daemon launch".to_string(), Mark::Warn)
            ]
        );
        assert!(r.checks[1].message.contains("runs from ssh"));
        let mut r = Report::new("mac", true);
        let ins = Inspected {
            gui: Some("113".into()),
            ..Inspected::default()
        };
        macos_checks(&mut r, "mac", &ins, &HubBuild::own(None), true);
        assert_eq!(r.checks[0].result, Mark::Fail);
        assert!(
            r.checks[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("automatic login")
                && r.checks[0]
                    .fix
                    .as_deref()
                    .unwrap()
                    .contains("`twr box restart mac`")
        );
        assert_eq!(r.checks[1].result, Mark::Warn);
        assert!(
            r.checks[1].message.contains("no hub runs"),
            "{:?}",
            r.checks
        );
        assert_eq!(r.failed(), 1);
        // The box offline: the hub's `launch` is the last daemon's, not known to be so now.
        let mut r = Report::new("mac", true);
        let ins = Inspected {
            gui: Some("0".into()),
            ..Inspected::default()
        };
        let offline = HubBuild::of_info(json!({"version": "0.2.0", "boxes": [
            {"name": "mac", "state": "offline", "launch": "launchd tools.twr.box.a.b",
             "os": "Darwin", "version": "0.2.0"}
        ]}));
        macos_checks(&mut r, "mac", &ins, &offline, true);
        assert_eq!(r.checks[1].result, Mark::Warn);
        assert_eq!(
            r.checks[1].message,
            "not known: the box is not connected (it is offline)"
        );
        // A failed `launchctl` is no answer, not "no one is logged in".
        let mut r = Report::new("mac", true);
        let ins = Inspected {
            gui: Some("127".into()),
            ..Inspected::default()
        };
        macos_checks(&mut r, "mac", &ins, &offline, true);
        assert_eq!(r.checks[0].result, Mark::Warn);
        assert!(
            r.checks[0].message.contains("couldn't tell"),
            "{:?}",
            r.checks
        );
        // A token box: a runner you start, no box daemon from ssh.
        for gui in ["0", "113"] {
            let mut r = Report::new("lab", true);
            let ins = Inspected {
                gui: Some(gui.into()),
                ..Inspected::default()
            };
            macos_checks(&mut r, "lab", &ins, &HubBuild::own(None), false);
            assert_eq!(r.checks.len(), 1, "{:?}", r.checks);
            let c = &r.checks[0];
            assert!(
                c.message.contains("a runner you start") && !c.message.contains("box daemon"),
                "{c:?}"
            );
        }
        let note = no_gui_login_note("mac", BoxTransport::Ssh);
        assert!(
            note.contains("No one is logged in at the Mac's screen")
                && note.contains("`twr box restart mac`"),
            "{note}"
        );
    }

    /// On a Mac, Claude Code's login is the box daemon's view (through the hub) when it has
    /// one, with a note when ssh's says otherwise (the keychain); other agents, and a box that
    /// is no Mac, keep the probe's.
    #[test]
    fn a_macs_claude_login_is_the_daemons() {
        let probe = json!({"agents": [
            {"runtime": "claude-code", "found": true, "login": "logged_out"},
            {"runtime": "codex", "found": true, "login": "logged_out"},
        ]});
        let side: Result<String, String> = Ok(format!("{AGENTS_MARK}\n{probe}\n{EXIT_MARK}0\n"));
        let hub = HubBuild::of_info(json!({"version": "0.2.0", "boxes": [
            {"name": "mac", "state": "online", "agents": [
                {"runtime": "claude-code", "found": true, "login": "logged_in",
                 "method": "claude.ai"},
                {"runtime": "codex", "found": true, "login": "logged_in"},
            ]}
        ]}));
        let pick = ("0123456789abcdef".to_string(), BuildSource::Hub);
        let daemon = DaemonView::Agents(
            super::super::box_agents(hub.box_entry("mac").unwrap()),
            None,
        );
        let mut r = Report::new("mac", true);
        agents_checks(&mut r, "mac", &hub, &daemon, Some(&pick), Some(&side), true);
        let agents: Vec<(String, Mark, String)> = r
            .checks
            .iter()
            .filter(|c| c.check == "agent")
            .map(|c| (c.label(), c.result, c.message.clone()))
            .collect();
        assert_eq!(agents.len(), 3, "{agents:?}");
        assert_eq!(agents[0].0, "claude");
        assert_eq!(agents[0].1, Mark::Ok);
        assert!(
            agents[0].2.ends_with("(as the box daemon reported it)"),
            "{agents:?}"
        );
        assert_eq!(agents[1].1, Mark::Warn);
        assert!(
            agents[1]
                .2
                .contains("over ssh it says logged out, the box daemon logged in")
                && agents[1].2.contains("keychain"),
            "{agents:?}"
        );
        assert_eq!(
            (agents[2].0.as_str(), agents[2].1),
            ("codex", Mark::Warn),
            "{agents:?}"
        );
        assert_eq!(r.agents.as_ref().unwrap()[0].login, AgentLogin::LoggedIn);
        // Not a Mac: the probe's.
        let mut r = Report::new("mac", true);
        agents_checks(
            &mut r,
            "mac",
            &hub,
            &DaemonView::NotAsked,
            Some(&pick),
            Some(&side),
            true,
        );
        let marks: Vec<Mark> = r
            .checks
            .iter()
            .filter(|c| c.check == "agent")
            .map(|c| c.result)
            .collect();
        assert_eq!(marks, [Mark::Warn, Mark::Warn]);
        // The box not connected: ssh's view, saying so (not what the hub kept).
        let mut r = Report::new("mac", true);
        let unknown = DaemonView::Unknown("the box is not connected (it is offline)".into());
        agents_checks(
            &mut r,
            "mac",
            &hub,
            &unknown,
            Some(&pick),
            Some(&side),
            true,
        );
        let claude = r.checks.iter().find(|c| c.check == "agent").unwrap();
        assert_eq!(claude.result, Mark::Warn);
        assert!(
            claude.message.starts_with("logged out")
                && claude.message.contains("over ssh")
                && claude
                    .message
                    .contains("how the box daemon sees it is not known, the box is not connected"),
            "{claude:?}"
        );
        assert_eq!(r.agents.as_ref().unwrap()[0].login, AgentLogin::LoggedOut);
        // The refresh failed: as the daemon last reported it, said so.
        let mut r = Report::new("mac", true);
        let stale = DaemonView::Agents(
            super::super::box_agents(hub.box_entry("mac").unwrap()),
            Some("no answer".into()),
        );
        agents_checks(&mut r, "mac", &hub, &stale, Some(&pick), Some(&side), true);
        let said: Vec<&str> = r
            .checks
            .iter()
            .filter(|c| c.check == "agent")
            .map(|c| c.message.as_str())
            .collect();
        assert!(said[0].contains("could not have box mac probe its agents now (no answer)"));
        assert!(
            said[1].ends_with("(as the box daemon last reported it)"),
            "{said:?}"
        );
        // A token box: its runner's view.
        let mut r = Report::new("mac", true);
        agents_checks(
            &mut r,
            "mac",
            &hub,
            &daemon,
            Some(&pick),
            Some(&side),
            false,
        );
        let said: Vec<&str> = r
            .checks
            .iter()
            .filter(|c| c.check == "agent")
            .map(|c| c.message.as_str())
            .collect();
        assert!(
            said[0].ends_with("(as the box's runner reported it)"),
            "{said:?}"
        );
        assert!(
            said[1].contains("the box's runner logged in") && !said[1].contains("daemon"),
            "{said:?}"
        );
    }

    /// A login that finds the CLI the daemon does not: an SSH box's daemon is restarted with
    /// `twr box restart`, a token box's runner by hand.
    #[test]
    fn a_daemon_that_misses_the_cli_is_restarted() {
        let ssh = daemon_missing_note(BoxTransport::Ssh, "mac", "claude");
        assert!(
            ssh.contains("To have it read the PATH again, run `twr box restart mac`")
                && ssh.ends_with("then `twr box doctor mac`")
                && !ssh.contains("--stop"),
            "{ssh}"
        );
        let token = daemon_missing_note(BoxTransport::Token, "lab", "claude");
        assert!(
            token.contains("restart the box's `twr runner`") && !token.contains("twr box restart"),
            "{token}"
        );
    }

    /// On a Mac, a daemon that does not find Claude Code is believed over ssh's probe that does
    /// (installed since the daemon started, on a PATH it does not have): a warning that says
    /// both, and `twr box restart`; when neither finds it, the usual install fix.
    #[test]
    fn a_macs_daemon_that_does_not_find_claude_is_believed() {
        let probe = json!({"agents": [
            {"runtime": "claude-code", "found": true, "login": "logged_in",
             "path": "/opt/homebrew/bin/claude"},
        ]});
        let side: Result<String, String> = Ok(format!("{AGENTS_MARK}\n{probe}\n{EXIT_MARK}0\n"));
        let hub = HubBuild::of_info(json!({"version": "0.2.0", "boxes": [
            {"name": "mac", "state": "online", "agents": [
                {"runtime": "claude-code", "found": false, "login": "unknown"},
            ]}
        ]}));
        let pick = ("0123456789abcdef".to_string(), BuildSource::Hub);
        let daemon = DaemonView::Agents(
            super::super::box_agents(hub.box_entry("mac").unwrap()),
            None,
        );
        let mut r = Report::new("mac", true);
        agents_checks(&mut r, "mac", &hub, &daemon, Some(&pick), Some(&side), true);
        let agents: Vec<_> = r.checks.iter().filter(|c| c.check == "agent").collect();
        assert_eq!(agents.len(), 1, "{:?}", r.checks);
        let c = agents[0];
        assert_eq!((c.label(), c.result), ("claude".to_string(), Mark::Warn));
        assert!(
            c.message
                .starts_with("not installed (as the box daemon reported it), but over ssh it is found (logged in")
                && c.message.contains("at /opt/homebrew/bin/claude"),
            "{}",
            c.message
        );
        assert_eq!(
            c.fix.as_deref(),
            Some("`twr box restart mac` starts a daemon that finds it")
        );
        assert!(!r.agents.as_ref().unwrap()[0].found);
        // Neither finds it: install it.
        let probe = json!({"agents": [
            {"runtime": "claude-code", "found": false, "login": "unknown"},
        ]});
        let side: Result<String, String> = Ok(format!("{AGENTS_MARK}\n{probe}\n{EXIT_MARK}0\n"));
        let mut r = Report::new("mac", true);
        agents_checks(&mut r, "mac", &hub, &daemon, Some(&pick), Some(&side), true);
        let c = r.checks.iter().find(|c| c.check == "agent").unwrap();
        assert!(
            c.message == "not installed (as the box daemon reported it)"
                && c.fix.as_deref().unwrap().contains("install claude there"),
            "{c:?}"
        );
    }
}

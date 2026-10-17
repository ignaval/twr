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
//!   second ssh in this terminal, within [`BOX_SIDE_WAIT`] ([`box_side_script`]).
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

/// How long `twr box login` waits for the hub to have the box probe its agents again (the
/// hub waits 30 s for the box).
const REFRESH_WAIT: Duration = Duration::from_secs(45);

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
    /// `login_shell`, `agent`, `git`, `disk`, `project`.
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
            other => other,
        };
        match (&self.item, self.check) {
            (Some(i), "agent") => i.clone(),
            (Some(i), _) => format!("{base} {i}"),
            (None, _) => base.to_string(),
        }
    }
}

/// Which build on the box runs the checks there and `twr box login` ([`pick_build`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildSource {
    /// The hub's own (the running hub's, else this twr's).
    Hub,
    /// The box's configured `build`.
    Configured,
    /// The newest build there.
    Newest,
}

impl BuildSource {
    fn said(self) -> &'static str {
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
async fn running_hub(socket: &Path) -> HubBuild {
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
/// its own: it may `exit`), then [`DOCTOR_MARK`] and `order=<build>` for each directory of the
/// builds tree, newest first. With `repos` (doctor), also: `linger=` / `kill=` (Linux:
/// logind's `Linger` of this user and `KillUserProcesses`, as the relay reads them), `git=`
/// (`git --version` on the PATH ssh gives, as the box daemon's), `df=` (`df -Pk` of the data
/// directory, or the nearest directory above it that exists), and `project.<i>=` (`git`, `notgit`
/// or `missing`) for each repository of `repos`, in order. Each of those commands is bounded
/// by [`BOX_COMMAND_SECS`] ([`bounded_prelude`]): one that hangs on the box (a `df` of a dead
/// network mount) ends with doctor, not after it.
fn inspect_script(repos: Option<&[String]>) -> String {
    inspect_script_bounded(repos, BOX_COMMAND_SECS)
}

/// How long each command doctor runs on the box may take, in seconds ([`inspect_script`]).
const BOX_COMMAND_SECS: u32 = 10;

/// The shell function `twr_b`, which runs its arguments as a command for at most `secs`
/// seconds: with `timeout` where the box has one that takes `timeout <secs> <command>` (GNU,
/// BusyBox), else in the background, killed by a watcher once `secs` pass. The watcher's
/// output goes to `/dev/null`, so a `$(...)` around `twr_b` does not wait for it.
fn bounded_prelude(secs: u32) -> String {
    format!(
        "if timeout 5 true >/dev/null 2>&1; then\n\
         \x20 twr_b() {{ timeout {secs} \"$@\"; }}\n\
         else\n\
         \x20 twr_b() {{\n\
         \x20   \"$@\" &\n\
         \x20   twr_p=$!\n\
         \x20   ( sleep {secs}; kill -9 \"$twr_p\" ) >/dev/null 2>&1 &\n\
         \x20   twr_w=$!\n\
         \x20   wait \"$twr_p\"; twr_s=$?\n\
         \x20   kill \"$twr_w\" >/dev/null 2>&1\n\
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
         case ${{XDG_DATA_HOME:-}} in /*) d=$XDG_DATA_HOME ;; *) d=${{HOME:-}}/.local/share ;; esac\n\
         ls -t \"$d/twr/runner/builds\" 2>/dev/null | sed 's/^/order=/'\n",
        probe = ssh_box::probe_script()
    );
    let Some(repos) = repos else {
        s.push_str("exit 0\n");
        return s;
    };
    s.push_str(&bounded_prelude(secs));
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
struct Inspected {
    probe: ssh_box::Probe,
    /// The builds tree's directories, newest first (not all of them builds twr trusts).
    newest_first: Vec<String>,
    /// logind's `Linger=yes|no` of this user (`none`: no `loginctl`), when asked (Linux).
    linger: Option<String>,
    /// `busctl`'s `KillUserProcesses` (`b true`), when there is `busctl` (Linux).
    kill: Option<String>,
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
fn parse_inspect(out: &str, projects: usize) -> anyhow::Result<Inspected> {
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
            "git" if ins.git.is_none() => ins.git = Some(cut(v)),
            "df" if ins.df.is_none() => ins.df = Some(cut(v)),
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
fn pick_build(
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
/// `systemctl`); with `agents`, `twr runner --agents` (one JSON line).
fn box_side_script(data: &str, id: &str, status: Option<(&str, &str)>, agents: bool) -> String {
    let exe = sh_word(&ssh_box::build_path(data, id));
    let mut s = String::new();
    if let Some((name, key)) = status {
        s.push_str(&format!(
            "echo '{STATUS_MARK}'\n\
             {exe} runner --stdio {} --hub-key {} --status </dev/null 2>/dev/null\n\
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
        s.push_str(&format!(
            "echo '{AGENTS_MARK}'\n\
             {exe} runner --agents </dev/null 2>/dev/null\n\
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
fn box_side_failure(id: &str, flag: &str, code: Option<i32>) -> String {
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
    agents_checks(&mut r, name, &hub, pick.as_ref(), side.as_ref());
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

/// The login shell's `PATH` and each agent CLI, from the agent probe in `side` (the second
/// ssh's output); without one, the agents as the box last reported them to the hub, when it
/// did.
fn agents_checks(
    r: &mut Report,
    name: &str,
    hub: &HubBuild,
    pick: Option<&(String, BuildSource)>,
    side: Option<&Result<String, String>>,
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
    for a in &report.agents {
        agent_check(r, name, a);
    }
    r.login_path = Some(report.login_path);
    r.agents = Some(report.agents);
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
    r.add_item("agent", Some(&command), mark, agent_said(a), fix);
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
            let restart = match (cfg.transport(), hub_key(paths)?) {
                (BoxTransport::Ssh, Some(key)) => format!(
                    "stop it with `{}` (its agents stop with it; the hub starts a new daemon \
                     when it connects again, within a minute, and their next prompts resume \
                     them)",
                    super::stop_command(ssh, &id, name, &key)?
                ),
                (BoxTransport::Ssh, None) => "stop it (`twr box remove` prints how), and the \
                    hub starts a new one when it connects again"
                    .to_string(),
                (BoxTransport::Token, _) => {
                    "restart the box's `twr runner` (its agents stop with it)".to_string()
                }
            };
            outln!(
                "the login found {command} on box {name}, but the box daemon doesn't: it reads \
                 the login shell's PATH once, when it starts, so a PATH entry added since is \
                 not in it. To have it read the PATH again, {restart}; then `twr box doctor \
                 {name}`"
            );
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
    }

    /// A box-side command that hangs (a `df` of a dead mount) is ended at the bound, with
    /// the box's `timeout` and without one: the script ends with doctor, not after it.
    #[test]
    fn the_box_side_commands_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let install = |bin: &std::path::Path, name: &str, body: &str| {
            std::fs::create_dir_all(bin).unwrap();
            let src = dir.path().join(format!("{name}.src"));
            std::fs::write(&src, format!("#!/bin/sh\n{body}\n")).unwrap();
            let st = std::process::Command::new("install")
                .args(["-m", "755"])
                .arg(&src)
                .arg(bin.join(name))
                .status()
                .unwrap();
            assert!(st.success());
        };
        let slow = dir.path().join("slow");
        install(&slow, "df", "exec sleep 30");
        // A box without a `timeout` that works.
        let no_timeout = dir.path().join("no-timeout");
        install(&no_timeout, "timeout", "exit 1");
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
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(
            &exe,
            "#!/bin/sh\ncase \"$*\" in\n\
             *--status) echo 'twr-box 1 00000000000000aa 7 0123456789abcdef' ;;\n\
             *--agents) echo '{\"login_path\":{\"error\":\"SHELL is not set\"},\"agents\":[{\"runtime\":\"codex\",\"found\":true,\"login\":\"logged_out\"}]}'; echo oops >&2 ;;\n\
             esac\n",
        )
        .unwrap();
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
            std::fs::write(&file, body).unwrap();
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
}

//! `twr box project <box> <project>`: where a project's repository is on a box, recorded as
//! `[boxes.<box>.projects.<project>] repo` in `boxes.toml`, so that `twr thread new <project>
//! --box <box>` finds it. Nothing is guessed at thread creation: what this finds is recorded
//! only once the user named it (`--path`) or confirmed it (or passed `--yes`).
//!
//! **Guards first.** An agent ([`refuse_agent_action`]) and a named workspace are refused
//! before any ssh or write; `--clone` needs a terminal on stdin and stdout (`--yes` only skips
//! its question).
//!
//! **The project, on the hub.** A configured project (`[projects.<p>] repo`), else a folder
//! name under the hub's `project_roots`, as `thread.new` looks it up on the hub's own box
//! ([`project_lookup`]). Its repository's folder name (one plain path component) is what the
//! box searches for, and its `origin` (read with twr's no-network git settings,
//! [`LocalGit`]) what a candidate must match. An origin holding credentials is compared
//! without them ([`RemoteUrl::parse_lenient`]), and they are never sent: the box gets the
//! URL without them.
//!
//! **On the box.** A first ssh in this terminal probes the box (the doctor's
//! [`inspect_script`]: its home, data directory and twr builds) and picks the build to run as
//! doctor picks it ([`pick_build`]). That build then runs `twr runner --find-repo` (every
//! operand quoted, [`sh_word`] inside [`ssh_box::sh_c`]) with the home-mapped hint: the hub
//! repository's path relative to the hub's home (`$HOME`), under the box's home. A candidate
//! is recorded by itself (after a y/N) only when it is the only one whose origin is the
//! project's and the search was complete; otherwise the candidates are listed (their origins
//! redacted) and `--path` picks one, or `--clone` makes one when there are none.
//!
//! **`--path`** is checked on the box the same way (`--find-repo` with it as the hint): it must
//! be a repository's top level.
//!
//! **`--clone`** refuses an origin twr does not clone ([`RemoteUrl::parse`]: credentials,
//! `file://`, a local path, another scheme), says why and what to do, and otherwise runs `twr
//! runner --clone-repo` on the box over ssh with a terminal ([`ssh_box::terminal_ssh_args`]:
//! forwarding off, so git uses the box's own credentials), in this terminal, into `--into`
//! (the clone's own new directory, as `git clone <url> <dir>` takes it: its parent and its
//! name are passed, [`split_into`]), else the home-mapped path (else `<box home>/code/<the
//! folder>`). The box prints the clone's path last (`twr-clone-path <path>`,
//! [`CLONE_PATH_MARK`]), which must end in that name, and is recorded. twr never fetches.
//!
//! **Recording** ([`write_entry`]): under the `boxes.toml` lock, only while the box's
//! connection settings and the project's entry (or its absence) are what they were when the
//! command started; for a box of `config.toml`, the lines to add there are printed. Then the
//! running hub reads the box's projects again (`box.projects`), and the command says whether
//! the entry is recorded only, or recorded and active: the hub's repository for the project,
//! which `box.projects` returns, is the one written ([`hub_took`]).
//!
//! **What the box says** (paths, origins) is shown with its control characters escaped, and
//! its origins [`redact`](twr_core::gitremote::redact)ed and cut again ([`shown_origin`]):
//! another build of twr may run there.

use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use twr_core::client::Client;
use twr_core::config::{BoxConfig, BoxProjectConfig, BoxesFile, Config};
use twr_core::gitremote::RemoteUrl;
use twr_core::paths::{sh_word, Paths};
use twr_core::project_lookup::{self, is_plain_name};
use twr_core::proto::{codes, RpcError};
use twr_hub::ssh_box;
use twr_runner::local::LocalGit;
use twr_runner::repos::{shown_origin, Candidate, FindReport, Reason, CLONE_PATH_MARK};

use crate::term::clean;

use super::doctor::{
    box_side_failure, inspect_script, parse_inspect, pick_build, running_hub, BuildSource,
};
use super::{
    ask_on_stderr, check_merged, confirm, consent, default_workspace_only, no_hub,
    refuse_agent_action, ssh_here, ssh_program_name, PROBE_WAIT,
};

/// How long the search on the box may take, ssh's questions included.
const FIND_WAIT: Duration = Duration::from_secs(120);

// The box side stops first ([`twr_runner::repos::FIND_DEADLINE`]), and says so.
const _: () = assert!(twr_runner::repos::FIND_DEADLINE.as_secs() + 15 < FIND_WAIT.as_secs());

/// How long the running hub may take to read the box's projects again (`box.projects`).
const HUB_WAIT: Duration = Duration::from_secs(15);

/// The most of the clone's output (the box's terminal) kept to find [`CLONE_PATH_MARK`] in.
const CLONE_TAIL_MAX: usize = 64 * 1024;

/// What `twr box project` was asked ([`super::BoxCmd::Project`]).
pub(super) struct Request {
    pub(super) name: String,
    pub(super) project: String,
    pub(super) path: Option<String>,
    pub(super) clone: bool,
    pub(super) into: Option<String>,
    pub(super) remove: bool,
    pub(super) yes: bool,
}

/// `twr box project` (see [`super::BoxCmd::Project`]).
pub(super) async fn project(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    req: Request,
) -> anyhow::Result<ExitCode> {
    refuse_agent_action("record projects on boxes")?;
    default_workspace_only(paths, "project")?;
    let name = req.name.as_str();
    twr_core::runner::validate_box_name(name)?;
    if req.clone {
        require_terminal(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )?;
    }
    let project = req.project.as_str();
    let Some(loaded) = config.boxes.get(name) else {
        bail!(
            "there is no box {name} in config.toml or boxes.toml (`twr box list` lists the \
             hub's boxes)"
        );
    };
    let file = paths.boxes_file();
    // One snapshot of the box, read from its file now, drives both the ssh below and the
    // write's comparison.
    let entry = Entry::new(&paths.config_file, &file, name, project, loaded)?;
    let cfg = &entry.start;
    if req.remove {
        return remove(&entry, socket).await;
    }
    if !config.projects.contains_key(project) && !is_plain_name(project) {
        bail!(
            "{project:?} is neither a configured project ([projects.<name>]) nor a repository \
             folder name (letters, digits, `-`, `_` and `.`)"
        );
    }
    let Some(ssh) = cfg.ssh.as_deref().filter(|s| !s.trim().is_empty()) else {
        bail!(
            "box {name} has no ssh destination ([boxes.{name}] ssh), so twr cannot look on it: \
             add [boxes.{name}.projects.{}] repo = \"<path on the box>\" yourself",
            toml_key(project)
        );
    };
    let hub_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute());
    let hub_project = resolve(config, project, hub_home.as_deref()).await?;
    // Checked before any ssh: an origin twr does not clone is refused at once.
    let clone_url = if req.clone {
        Some(clone_url(&hub_project, name, project)?)
    } else {
        None
    };
    for p in [&req.path, &req.into].into_iter().flatten() {
        check_box_path(p)?;
    }
    let into = req.into.as_deref().map(split_into).transpose()?;
    let hub = running_hub(socket).await;
    let side = probe(name, ssh, cfg, &hub).await?;
    let mapped = side
        .home
        .as_deref()
        .and_then(|h| home_mapped(&hub_project.repo, hub_home.as_deref(), h));
    if let Some(url) = clone_url {
        return clone(
            &entry,
            socket,
            ssh,
            &side,
            &hub_project,
            &url,
            mapped.as_deref(),
            into,
            req.yes,
        )
        .await;
    }
    if let Some(path) = &req.path {
        return by_path(&entry, socket, ssh, &side, &hub_project, path).await;
    }
    search(
        &entry,
        socket,
        ssh,
        &side,
        &hub_project,
        mapped.as_deref(),
        req.yes,
    )
    .await
}

/// Refuses `twr box project --clone` without a terminal on stdin (`stdin`) and stdout
/// (`stdout`): git may ask for what it needs there.
fn require_terminal(stdin: bool, stdout: bool) -> anyhow::Result<()> {
    if stdin && stdout {
        return Ok(());
    }
    bail!(
        "`twr box project --clone` needs a terminal on stdin and stdout: git runs on the box in \
         it and may ask you things there (--yes only skips twr's own question). Run it from \
         your own terminal"
    )
}

/// `s` as a TOML key (bare when it can be).
fn toml_key(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        s.to_string()
    } else {
        toml_str(s)
    }
}

/// `s` as a TOML basic string: JSON's quoting, which is TOML's for text without a control
/// character (the paths and names here have none; a key may hold one only as configured).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A path on the box as the user gave it (`--path`, `--into`): absolute (a `~` would be this
/// machine's home once the shell expanded it, or no path at all), printable.
fn check_box_path(p: &str) -> anyhow::Result<()> {
    if p.chars().any(char::is_control) {
        bail!("{p:?}: a path on the box must be printable");
    }
    if !p.starts_with('/') {
        bail!(
            "{p:?} is not an absolute path on the box (write the box's home out, as in \
             /home/me/code)"
        );
    }
    Ok(())
}

/// A project on the hub ([`resolve`]).
#[derive(Debug)]
struct HubProject {
    /// Its repository on the hub's machine (as configured, or found).
    repo: PathBuf,
    /// The repository's folder name: what the box searches for.
    folder: String,
    /// Its `origin` without credentials, when it has one twr can compare.
    origin: Option<RemoteUrl>,
    /// Why there is no origin to clone, when there is none ([`RemoteUrl::parse`]'s refusal,
    /// say).
    no_clone: Option<String>,
}

/// Project `project` on the hub (see the module docs), `home` the hub's home directory. Says
/// what it found, and what it does with an origin holding credentials.
async fn resolve(
    config: &Config,
    project: &str,
    home: Option<&Path>,
) -> anyhow::Result<HubProject> {
    let repo = match config.projects.get(project) {
        Some(p) => {
            let Some(repo) = twr_core::config::expand_home(&p.repo, home) else {
                bail!(
                    "[projects.{}] repo {} starts with `~`, and HOME is not set",
                    toml_key(project),
                    p.repo.display()
                );
            };
            if !repo.is_absolute() {
                bail!(
                    "[projects.{}] repo {} is not an absolute path",
                    toml_key(project),
                    repo.display()
                );
            }
            std::fs::canonicalize(&repo).unwrap_or(repo)
        }
        None => find_on_hub(config, project, home).await?,
    };
    let folder = repo
        .file_name()
        .and_then(|f| f.to_str())
        .filter(|f| is_plain_name(f))
        .ok_or_else(|| {
            anyhow!(
                "the folder name of {} is not one twr searches a box for (letters, digits, `-`, \
                 `_` and `.`): add [boxes.<box>.projects.{}] repo = \"<path on the box>\" \
                 yourself",
                repo.display(),
                toml_key(project)
            )
        })?
        .to_string();
    // (`Err`: git could not read it, which is not having none.)
    let raw = if repo.is_dir() {
        match LocalGit::default().origin_url(&repo).await {
            Ok(o) => Ok(o),
            Err(e) => {
                errln!(
                    "twr: warning: could not read the origin of {}: {e:#}",
                    repo.display()
                );
                Err(format!(
                    "git could not read the origin of {}",
                    repo.display()
                ))
            }
        }
    } else {
        outln!(
            "project {project}: {} is not on this machine, so it has no origin to compare",
            repo.display()
        );
        Ok(None)
    };
    let (origin, no_clone) = match &raw {
        Err(why) => (None, Some(why.clone())),
        Ok(None) => (
            None,
            Some(format!("{} has no origin remote", repo.display())),
        ),
        Ok(Some(raw)) => match RemoteUrl::parse_lenient(raw) {
            Ok(u) => {
                let no_clone = RemoteUrl::parse(raw).err();
                if u.had_credentials() {
                    outln!(
                        "the origin of {} holds a password or a token: twr compares it without \
                         them ({}) and never sends them to the box",
                        repo.display(),
                        u.redacted()
                    );
                }
                (Some(u), no_clone)
            }
            Err(why) => {
                outln!("the origin of {} is not compared: {why}", repo.display());
                (None, Some(why))
            }
        },
    };
    outln!(
        "project {project}: {} on this machine, origin {}",
        repo.display(),
        origin
            .as_ref()
            .map_or_else(|| "none".to_string(), RemoteUrl::redacted)
    );
    Ok(HubProject {
        repo,
        folder,
        origin,
        no_clone,
    })
}

/// The repository folder `name` stands for under the hub's `project_roots` (`home` the hub's
/// home): exactly one, from a complete search, else an error saying what to do.
async fn find_on_hub(config: &Config, name: &str, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let roots = config.project_roots(home);
    if roots.is_empty() {
        bail!(
            "{name:?} is not a configured project, and project_roots is empty (folder names are \
             not looked up): add [projects.{name}] repo = \"/path/to/{name}\" to the twr config"
        );
    }
    let found = {
        let (roots, name) = (roots.clone(), name.to_string());
        tokio::task::spawn_blocking(move || project_lookup::find(&roots, &name))
            .await
            .context("searching project_roots")?
    };
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Why the search may have missed one.
    let missed = match (found.truncated, found.unreadable) {
        (true, _) => Some("stopped early (too many folders)"),
        (false, true) => Some("could not read a folder"),
        (false, false) => None,
    };
    match (found.repos.as_slice(), missed) {
        ([one], None) => Ok(one.clone()),
        ([], _) => bail!(
            "{name:?} is not a configured project, and no git repository named {name:?} is \
             under project_roots on this machine ({}){}: add [projects.{name}] repo = \
             \"/path/to/{name}\" to the twr config",
            list(&roots),
            missed.map_or_else(String::new, |m| format!(" (the search {m})"))
        ),
        ([one], Some(missed)) => bail!(
            "found {} for {name:?}, but the search of project_roots {missed}, so another of \
             that name may exist: add [projects.{name}] repo = \"{}\" to the twr config if it \
             is the one",
            one.display(),
            one.display()
        ),
        (many, _) => bail!(
            "{name:?} names several git repositories on this machine: {}. Add [projects.{name}] \
             repo = \"<the one>\" to the twr config",
            list(many)
        ),
    }
}

/// The URL `--clone` clones for `p` (project `project` of box `name`), or why not and what to
/// do instead.
fn clone_url(p: &HubProject, name: &str, project: &str) -> anyhow::Result<RemoteUrl> {
    match (&p.origin, &p.no_clone) {
        (Some(u), None) => Ok(u.clone()),
        (_, why) => bail!(
            "twr does not clone the origin of project {project}: {}. Clone it on box {name} \
             yourself, then record it with `twr box project {} {} --path <path on the box>`; or \
             set the repository's origin to an https://, ssh:// or user@host:path URL without \
             credentials",
            why.as_deref().unwrap_or("it has none"),
            sh_word(name),
            sh_word(project)
        ),
    }
}

/// `repo`'s path relative to the hub's home `home`, under the box's home `box_home`: the place
/// the box most likely has it. `None` when `repo` is not under `home` (compared component by
/// component, as given and canonical), or the path is not printable UTF-8.
fn home_mapped(repo: &Path, home: Option<&Path>, box_home: &str) -> Option<String> {
    let home = home?;
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let rel = [
        (repo.to_path_buf(), home.to_path_buf()),
        (canonical(repo), canonical(home)),
    ]
    .into_iter()
    .find_map(|(r, h)| {
        let rel = r.strip_prefix(&h).ok()?;
        let parts: Option<Vec<&str>> = rel
            .components()
            .map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        parts.filter(|p| !p.is_empty()).map(|p| p.join("/"))
    })?;
    if rel.chars().any(char::is_control) || !box_home.starts_with('/') {
        return None;
    }
    Some(format!("{}/{rel}", box_home.trim_end_matches('/')))
}

/// What the first ssh found on the box ([`probe`]).
struct BoxSide {
    /// The box's name.
    name: String,
    /// Its data directory.
    data: String,
    /// The build that runs there, and why that one.
    id: String,
    /// The box's home, when absolute.
    home: Option<String>,
}

impl BoxSide {
    /// The box-side command `twr runner <args>` of the build picked, each argument quoted.
    fn runner(&self, args: &[String]) -> String {
        let mut s = format!(
            "exec {} runner",
            sh_word(&ssh_box::build_path(&self.data, &self.id))
        );
        for a in args {
            s.push(' ');
            s.push_str(&sh_word(a));
        }
        s
    }
}

/// Probes box `name` (`cfg`, ssh `ssh`) in this terminal and picks the twr build that runs
/// there, as `twr box doctor` picks it (the running hub's `hub` first).
async fn probe(
    name: &str,
    ssh: &str,
    cfg: &BoxConfig,
    hub: &super::HubBuild,
) -> anyhow::Result<BoxSide> {
    outln!("probing box {name} over ssh ({ssh})...");
    let remote = ssh_box::sh_c(&inspect_script(None));
    let (code, out) = tokio::time::timeout(PROBE_WAIT, ssh_here(ssh, &remote, None))
        .await
        .map_err(|_| {
            anyhow!(
                "probing box {name} did not finish within {} s (a question of ssh's left \
                 unanswered, or a box that does not answer); nothing was changed",
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
            "box {name} has no twr build to look for the repository with: `twr box install \
             {name}` (an SSH box of the hub's OS and CPU gets the hub's as the hub connects)"
        );
    };
    let why = match (&hub.unknown, source) {
        (Some(why), BuildSource::Configured | BuildSource::Newest) => {
            format!(" (the running hub did not say which build it runs: {why})")
        }
        _ => String::new(),
    };
    outln!("box {name} runs twr with {} {id}{why}", source.said());
    Ok(BoxSide {
        name: name.to_string(),
        data,
        id,
        home: ins.home,
    })
}

/// `twr runner --find-repo` on the box (see the module docs): its report, the candidates
/// whose path is not absolute and printable left out (it is not complete then).
async fn find_on_box(
    ssh: &str,
    side: &BoxSide,
    folder: &str,
    origin: Option<&RemoteUrl>,
    hint: Option<&str>,
) -> anyhow::Result<FindReport> {
    let name = &side.name;
    let mut args = vec!["--find-repo".to_string(), format!("--name={folder}")];
    if let Some(o) = origin {
        args.push(format!("--origin={}", o.redacted()));
    }
    if let Some(h) = hint {
        args.push(format!("--hint={h}"));
    }
    let script = format!("{} </dev/null\n", side.runner(&args));
    let (code, out) = tokio::time::timeout(FIND_WAIT, ssh_here(ssh, &ssh_box::sh_c(&script), None))
        .await
        .map_err(|_| {
            anyhow!(
                "the search on box {name} did not finish within {} s; nothing was changed",
                FIND_WAIT.as_secs()
            )
        })??;
    if code != Some(0) {
        bail!(
            "{} on box {name}: see its message above; nothing was changed",
            box_side_failure(&side.id, "--find-repo", code)
        );
    }
    let Some(mut report) = out
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<FindReport>(l.trim()).ok())
    else {
        bail!("the search on box {name} printed no report twr understands");
    };
    let reported = report.candidates.len();
    report
        .candidates
        .retain(|c| c.path.starts_with('/') && !c.path.chars().any(char::is_control));
    report
        .candidates
        .truncate(twr_runner::repos::MAX_CANDIDATES);
    // One left out may have been the project's.
    report.complete &= report.candidates.len() == reported;
    refuse_without_git(name, &report)?;
    Ok(report)
}

/// Refuses `report` of box `name` when the box has no git: nothing there could be checked.
fn refuse_without_git(name: &str, report: &FindReport) -> anyhow::Result<()> {
    if report.git_missing {
        bail!(
            "git is not installed on box {name} (twr found no `git` on the PATH ssh gives it \
             there), so no folder there could be checked: install git on the box, then run \
             this again; nothing was recorded"
        );
    }
    Ok(())
}

/// Why `report`, a search on box `name`, is not complete, as said to the user.
fn incomplete_why(name: &str, report: &FindReport) -> String {
    if report.home.is_none() {
        format!(
            "the search could not run: box {name} has no absolute HOME, so its project folders \
             (code, src, ...) were not searched"
        )
    } else if report.timed_out {
        format!(
            "the search was incomplete: it stopped after {} s on box {name}",
            twr_runner::repos::FIND_DEADLINE.as_secs()
        )
    } else {
        "the search was incomplete (too many folders or repositories, or one twr could not \
         read)"
            .to_string()
    }
}

/// One candidate, as listed (what the box said of it shown as twr shows it: see the module
/// docs).
fn candidate_line(c: &Candidate) -> String {
    let mut notes = vec![
        match (c.origin.as_deref().map(box_origin), c.matches_origin) {
            (Some(o), true) => format!("origin {o}, the project's"),
            (Some(o), false) => format!("origin {o}"),
            (None, _) if c.origin_unread => "its origin could not be read".to_string(),
            (None, _) => "no origin".to_string(),
        },
    ];
    if c.reason == Reason::Hint {
        notes.push("where it is under your home here".to_string());
    }
    if !c.toplevel {
        notes.push("inside a repository, not its top level".to_string());
    }
    format!("  {} ({})", clean(&c.path), notes.join("; "))
}

/// An `origin` the box reported, as shown here: [`shown_origin`] (redacted, cut) and
/// [`clean`].
fn box_origin(o: &str) -> String {
    clean(&shown_origin(o)).into_owned()
}

/// The default: search the box, record the one candidate with the project's origin from a
/// complete search (after a y/N unless `yes`), else list them.
async fn search(
    entry: &Entry<'_>,
    socket: &Path,
    ssh: &str,
    side: &BoxSide,
    p: &HubProject,
    hint: Option<&str>,
    yes: bool,
) -> anyhow::Result<ExitCode> {
    let (name, project) = (entry.box_name, entry.project);
    match (hint, side.home.as_deref()) {
        (Some(h), Some(home)) => outln!(
            "looking on box {name} for repository folder {}: at {h} (where it is under your home \
             here), then under the project folders in {home} (code, src, ...)",
            p.folder
        ),
        (None, Some(home)) => outln!(
            "looking on box {name} for repository folder {} under the project folders in {home} \
             (code, src, ...)",
            p.folder
        ),
        (_, None) => outln!(
            "box {name} has no absolute HOME, so twr cannot search its project folders for {}",
            p.folder
        ),
    }
    let report = find_on_box(ssh, side, &p.folder, p.origin.as_ref(), hint).await?;
    let again = |opt: &str| {
        format!(
            "`twr box project {} {} {opt}`",
            sh_word(name),
            sh_word(project)
        )
    };
    if report.candidates.is_empty() {
        let stopped = if report.complete {
            String::new()
        } else {
            format!(" ({})", incomplete_why(name, &report))
        };
        outln!(
            "no repository {} found on box {name}{stopped}; nothing was recorded",
            p.folder
        );
        match &p.no_clone {
            None => outln!(
                "clone it there with {} (git runs on the box, in this terminal, with the box's \
                 own credentials), or give its path with {}",
                again("--clone"),
                again("--path <path on the box>")
            ),
            Some(_) => outln!(
                "give its path with {} once it is there",
                again("--path <path on the box>")
            ),
        }
        return Ok(ExitCode::from(1));
    }
    outln!("found on box {name}:");
    for c in &report.candidates {
        outln!("{}", candidate_line(c));
    }
    let matching: Vec<&Candidate> = report
        .candidates
        .iter()
        .filter(|c| c.matches_origin && c.toplevel)
        .collect();
    let one = match (matching.as_slice(), report.complete, &p.origin) {
        ([one], true, Some(_)) => Some(*one),
        _ => None,
    };
    let Some(one) = one else {
        let why = if p.origin.is_none() {
            "the project has no origin to compare them with".to_string()
        } else if !report.complete {
            format!("{}, so another may match", incomplete_why(name, &report))
        } else if matching.is_empty() {
            "none has the project's origin".to_string()
        } else {
            format!("{} have the project's origin", matching.len())
        };
        outln!(
            "nothing was recorded: {why}. Pick one with {}",
            again("--path <path>")
        );
        return Ok(ExitCode::from(1));
    };
    let path = one.path.as_str();
    if entry.recorded() == Some(Path::new(path)) {
        outln!("project {project} on box {name} is recorded as {path} already");
        tell_hub(entry, socket, Some(path)).await;
        return Ok(ExitCode::SUCCESS);
    }
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if let Some(old) = entry.recorded() {
        outln!("(it replaces {}, recorded now)", old.display());
    }
    if !confirm(
        consent,
        &format!("Record {path} as project {project} on box {name}? [y/N] "),
        &format!("not recording project {project} on box {name} without confirmation"),
    )? {
        outln!("nothing was recorded");
        return Ok(ExitCode::SUCCESS);
    }
    record(entry, socket, path).await?;
    Ok(ExitCode::SUCCESS)
}

/// `--path`: `path` checked on the box (a repository's top level), then recorded.
async fn by_path(
    entry: &Entry<'_>,
    socket: &Path,
    ssh: &str,
    side: &BoxSide,
    p: &HubProject,
    path: &str,
) -> anyhow::Result<ExitCode> {
    let name = entry.box_name;
    outln!("checking {path} on box {name}...");
    let report = find_on_box(ssh, side, &p.folder, p.origin.as_ref(), Some(path)).await?;
    let Some(c) = report.candidates.iter().find(|c| c.reason == Reason::Hint) else {
        bail!(
            "{path} on box {name} is not in a git repository (or is not there): nothing was \
             recorded"
        );
    };
    if !c.toplevel {
        bail!(
            "{path} on box {name} is inside a git repository, not its top level: give the \
             repository's top level; nothing was recorded"
        );
    }
    if let Some(want) = p.origin.as_ref().filter(|_| !c.matches_origin) {
        errln!(
            "twr: warning: {} has {}, not the project's origin {}: recorded as you named it",
            clean(&c.path),
            c.origin.as_deref().map_or_else(
                || "no origin".to_string(),
                |o| format!("origin {}", box_origin(o))
            ),
            want.redacted()
        );
    }
    if entry.recorded() == Some(Path::new(&c.path)) {
        outln!(
            "project {} on box {name} is recorded as {} already",
            entry.project,
            c.path
        );
        tell_hub(entry, socket, Some(&c.path)).await;
        return Ok(ExitCode::SUCCESS);
    }
    record(entry, socket, &c.path).await?;
    Ok(ExitCode::SUCCESS)
}

/// `--clone`: `url` cloned on the box in this terminal (see the module docs) into `into` (the
/// clone's own directory, as its parent and name: [`split_into`]) or the default target, then
/// recorded.
#[allow(clippy::too_many_arguments)]
async fn clone(
    entry: &Entry<'_>,
    socket: &Path,
    ssh: &str,
    side: &BoxSide,
    p: &HubProject,
    url: &RemoteUrl,
    mapped: Option<&str>,
    into: Option<(String, String)>,
    yes: bool,
) -> anyhow::Result<ExitCode> {
    let (name, project) = (entry.box_name, entry.project);
    let (parent, folder) = match into {
        Some(into) => into,
        None => {
            let parent = clone_parent(mapped, side.home.as_deref()).ok_or_else(|| {
                anyhow!(
                    "box {name} has no absolute HOME to clone under: name the clone's directory \
                     with --into <new dir on the box>"
                )
            })?;
            (parent, p.folder.clone())
        }
    };
    let target = format!("{}/{folder}", parent.trim_end_matches('/'));
    outln!(
        "twr clones {} into {target} on box {name}: git runs on the box, in this terminal, with \
         the box's own git credentials (its ssh keys and credential helpers); twr forwards \
         neither your ssh agent nor any credential. twr never fetches into it afterwards.",
        url.redacted()
    );
    if let Some(old) = entry.recorded() {
        outln!(
            "(project {project} on box {name} is recorded as {} now: the clone replaces it)",
            old.display()
        );
    }
    let mut ask = ask_on_stderr;
    if !confirm(
        consent(yes, &mut ask),
        &format!("Clone it on box {name}? [y/N] "),
        &format!("not cloning on box {name} without confirmation"),
    )? {
        outln!("nothing was cloned");
        return Ok(ExitCode::SUCCESS);
    }
    let args = clone_args(ssh, side, url, &parent, &folder)?;
    let (status, tail) = run_in_terminal(&args).await?;
    let path = clone_path(&tail, &folder);
    let path = match (status.code(), path) {
        (Some(0), Some(path)) => path,
        (Some(0), None) => bail!(
            "the clone on box {name} ended without saying where it is: nothing was recorded; \
             once it is there, `twr box project {} {} --path <path>` records it",
            sh_word(name),
            sh_word(project)
        ),
        (code, _) => bail!(
            "{} on box {name}: see its message above; nothing was recorded",
            box_side_failure(&side.id, "--clone-repo", code)
        ),
    };
    outln!("cloned {} into {path} on box {name}", url.redacted());
    record(entry, socket, &path).await?;
    Ok(ExitCode::SUCCESS)
}

/// `--into <dir>` (absolute and printable: [`check_box_path`]), the clone's own directory, as
/// `git clone <url> <dir>` takes it: (its parent, which must exist on the box, and its last
/// component, a plain folder name the box makes).
fn split_into(into: &str) -> anyhow::Result<(String, String)> {
    let trimmed = into.trim_end_matches('/');
    let (parent, last) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
    if !is_plain_name(last) {
        bail!(
            "--into {into:?} names the clone's own new directory, and {last:?} is not a folder \
             name twr makes (letters, digits, `-`, `_` and `.`, not starting with `.` or `-`)"
        );
    }
    let parent = parent.trim_end_matches('/');
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent.to_string(), last.to_string()))
}

/// The directory a clone without `--into` goes in (named after the repository's folder): the
/// parent of the home-mapped path `mapped`, else `code` under the box's home `home`.
fn clone_parent(mapped: Option<&str>, home: Option<&str>) -> Option<String> {
    if let Some(parent) = mapped
        .and_then(|m| Path::new(m).parent())
        .and_then(Path::to_str)
    {
        return Some(parent.to_string());
    }
    home.filter(|h| h.starts_with('/'))
        .map(|h| format!("{}/code", h.trim_end_matches('/')))
}

/// The ssh arguments of a clone: with a terminal ([`ssh_box::terminal_ssh_args`]), the
/// build picked runs `twr runner --clone-repo --url=<url> --parent=<parent> --name=<folder>`
/// (`<folder>` the clone's own directory's name).
fn clone_args(
    ssh: &str,
    side: &BoxSide,
    url: &RemoteUrl,
    parent: &str,
    folder: &str,
) -> anyhow::Result<Vec<String>> {
    let remote = ssh_box::sh_c(&side.runner(&[
        "--clone-repo".to_string(),
        format!("--url={}", url.redacted()),
        format!("--parent={parent}"),
        format!("--name={folder}"),
    ]));
    ssh_box::terminal_ssh_args(ssh, &remote).map_err(|why| anyhow!("ssh {ssh:?}: {why}"))
}

/// Runs ssh with `args` (a terminal on the box) with this terminal as its stdin and stderr,
/// its stdout (the box's terminal) copied here as it comes, as it is: its exit status and the
/// last [`CLONE_TAIL_MAX`] bytes of that output.
async fn run_in_terminal(args: &[String]) -> anyhow::Result<(std::process::ExitStatus, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(ssh_box::ssh_program())
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {}", ssh_program_name()))?;
    let mut from = child.stdout.take().expect("piped");
    let mut out = std::io::stdout();
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // (This terminal gone: the clone goes on; its path is still read.)
        let _ = out.write_all(&buf[..n]).and_then(|()| out.flush());
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > 2 * CLONE_TAIL_MAX {
            tail.drain(..tail.len() - CLONE_TAIL_MAX);
        }
    }
    let status = child
        .wait()
        .await
        .with_context(|| format!("running {}", ssh_program_name()))?;
    Ok((status, tail))
}

/// The path in the last `twr-clone-path <path>` line of `out` ([`CLONE_PATH_MARK`]): an
/// absolute, printable path ending in `/<folder>`.
fn clone_path(out: &[u8], folder: &str) -> Option<String> {
    let text = String::from_utf8_lossy(out);
    let path = text.lines().rev().find_map(|l| {
        l.trim_end_matches('\r')
            .strip_prefix(CLONE_PATH_MARK)?
            .strip_prefix(' ')
    })?;
    let ok = path.starts_with('/')
        && !path.chars().any(char::is_control)
        && Path::new(path).file_name().and_then(|f| f.to_str()) == Some(folder);
    ok.then(|| path.to_string())
}

/// The project entry a command changes, and what it starts from.
struct Entry<'a> {
    config_file: &'a Path,
    /// `boxes.toml`.
    file: &'a Path,
    box_name: &'a str,
    project: &'a str,
    /// The box as it was when the command started (boxes.toml's, else config.toml's).
    start: BoxConfig,
    /// The box is in boxes.toml (else in config.toml, which twr does not edit).
    in_file: bool,
}

impl<'a> Entry<'a> {
    /// Box `box_name`'s project `project` as its file has it now (boxes.toml, else
    /// config.toml): the one snapshot the command works from, its ssh to the box and its write
    /// both. Refused when the box's connection settings there are not those of `loaded` (the
    /// box as the command's config was loaded, earlier), or the box is in neither file now.
    fn new(
        config_file: &'a Path,
        file: &'a Path,
        box_name: &'a str,
        project: &'a str,
        loaded: &BoxConfig,
    ) -> anyhow::Result<Entry<'a>> {
        let (start, in_file) = super::box_snapshot(config_file, file, box_name, loaded)?;
        Ok(Entry {
            config_file,
            file,
            box_name,
            project,
            start,
            in_file,
        })
    }

    /// The project's repository on the box as recorded when the command started.
    fn recorded(&self) -> Option<&Path> {
        self.start
            .projects
            .get(self.project)
            .map(|p| p.repo.as_path())
    }

    /// `[boxes.<box>.projects.<project>]`.
    fn table(&self) -> String {
        format!(
            "[boxes.{}.projects.{}]",
            toml_key(self.box_name),
            toml_key(self.project)
        )
    }
}

/// Sets (`repo`) or removes (`None`) `entry`'s project entry in boxes.toml, under its lock,
/// only while the box's connection settings and that entry are as `entry.start` has them
/// (another change meanwhile is refused, the file left as it is); the rest of the file as it
/// is. The entry's other keys (its `runtime`) are kept.
fn write_entry(entry: &Entry<'_>, repo: Option<&str>) -> anyhow::Result<()> {
    let (name, project, file) = (entry.box_name, entry.project, entry.file);
    let _lock = BoxesFile::lock(file)?;
    let mut boxes = BoxesFile::load(file)?;
    let Some(b) = boxes.boxes.get_mut(name) else {
        bail!(
            "box {name} was removed from {} meanwhile: nothing was recorded",
            file.display()
        );
    };
    if b.connection() != entry.start.connection() {
        bail!(
            "box {name} changed in {} meanwhile (its ssh destination or settings): nothing was \
             recorded; run the command again",
            file.display()
        );
    }
    let prior = entry.start.projects.get(project);
    if b.projects.get(project) != prior {
        let now = b
            .projects
            .get(project)
            .map_or_else(|| "none".to_string(), |p| p.repo.display().to_string());
        bail!(
            "{} changed in {} meanwhile (its repo is {now} now): nothing was recorded; run the \
             command again",
            entry.table(),
            file.display()
        );
    }
    match repo {
        Some(r) => {
            b.projects.insert(
                project.to_string(),
                BoxProjectConfig {
                    repo: PathBuf::from(r),
                    ..prior.cloned().unwrap_or_default()
                },
            );
        }
        None => {
            b.projects.remove(project);
        }
    }
    check_merged(&Config::load(entry.config_file)?, &boxes, file)?;
    boxes.save(file)
}

/// Records `path` as `entry`'s repository ([`write_entry`]; a box of config.toml: the lines to
/// add there are printed), then has the running hub take it ([`tell_hub`]).
async fn record(entry: &Entry<'_>, socket: &Path, path: &str) -> anyhow::Result<()> {
    let (name, project) = (entry.box_name, entry.project);
    if !entry.in_file {
        outln!(
            "box {name} is configured in {}, which twr does not edit: add these lines there",
            entry.config_file.display()
        );
        outln!("{}", entry.table());
        outln!("repo = {}", toml_str(path));
        outln!(
            "then run `twr box project {} {} --path {}` again to have the running hub take it",
            sh_word(name),
            sh_word(project),
            sh_word(path)
        );
        return Ok(());
    }
    write_entry(entry, Some(path))?;
    tell_hub(entry, socket, Some(path)).await;
    Ok(())
}

/// `--remove`: takes `entry` out of boxes.toml (a box of config.toml: says what to remove
/// there), then has the running hub take that.
async fn remove(entry: &Entry<'_>, socket: &Path) -> anyhow::Result<ExitCode> {
    let name = entry.box_name;
    let Some(old) = entry.recorded() else {
        bail!("box {name} has no {} to remove", entry.table());
    };
    if !entry.in_file {
        bail!(
            "box {name} is configured in {}, which twr does not edit: remove its {} there",
            entry.config_file.display(),
            entry.table()
        );
    }
    let old = old.display().to_string();
    write_entry(entry, None)?;
    outln!(
        "removed {} (repo {old}) from {}",
        entry.table(),
        entry.file.display()
    );
    tell_hub(entry, socket, None).await;
    Ok(ExitCode::SUCCESS)
}

/// Has the hub on `socket`, when one runs, read box `entry`'s projects again (`box.projects`),
/// and says whether the change (`path` recorded; `None`: removed) is active: the hub's
/// repository for the project now is that one ([`hub_took`]).
async fn tell_hub(entry: &Entry<'_>, socket: &Path, path: Option<&str>) {
    let (name, project) = (entry.box_name, entry.project);
    let what = match path {
        Some(p) => format!("project {project} on box {name} is {p}"),
        None => format!("project {project} on box {name} is no longer recorded"),
    };
    let use_it = format!(
        "`twr thread new {} --box {}`",
        sh_word(project),
        sh_word(name)
    );
    let done = if path.is_some() {
        "recorded"
    } else {
        "removed"
    };
    match tokio::time::timeout(HUB_WAIT, reload(socket, name)).await {
        Ok(Ok(Some(result))) => match hub_took(&result, project, path) {
            HubTook::That => match path {
                Some(_) => outln!("recorded and active: {what}; {use_it} uses it"),
                None => outln!("removed and active: {what}"),
            },
            HubTook::Other(other) => {
                outln!("{done}: {what}");
                outln!(
                    "but the running hub has {} for it: the box's projects changed in {} again \
                     before the hub read them (another `twr box project`, or an edit), and the \
                     hub has what the file says now; run `twr box project {} {}` again to see",
                    other.map_or_else(
                        || "no repository".to_string(),
                        |o| format!("repo {}", clean(&o))
                    ),
                    entry.file.display(),
                    sh_word(name),
                    sh_word(project)
                );
            }
            HubTook::Unknown => {
                outln!("{done}: {what}");
                outln!(
                    "the running hub read the box's projects again but did not say which \
                     repository it has for {project} (an older hub): restart it (`twr hub \
                     stop`, then `twr`) to be sure it has this one"
                );
            }
        },
        Ok(Ok(None)) => outln!("{done}: {what} (no hub runs: one started later reads it)"),
        Ok(Err(why)) => {
            outln!("{done}: {what}");
            outln!("not active yet: {why}");
        }
        Err(_) => {
            outln!("{done}: {what}");
            outln!(
                "not active yet: the running hub did not answer within {} s; restart it (`twr \
                 hub stop`, then `twr`) for it to read the change",
                HUB_WAIT.as_secs()
            );
        }
    }
}

/// What the hub took for a project, by its `box.projects` result ([`hub_took`]).
#[derive(Debug, PartialEq)]
enum HubTook {
    /// What was written: the repository (or none, removed).
    That,
    /// Another repository (`None`: none).
    Other(Option<String>),
    /// The result says nothing of it (an older hub, without `repos`).
    Unknown,
}

/// Whether `result`, the hub's `box.projects` result (`repos`: each project's repo), has
/// `path` for `project` (`None`: no entry, removed).
fn hub_took(result: &Value, project: &str, path: Option<&str>) -> HubTook {
    let Some(repos) = result.get("repos").and_then(Value::as_object) else {
        return HubTook::Unknown;
    };
    let has = repos.get(project).map(|r| r.as_str().map(str::to_string));
    match (has, path) {
        (None, None) => HubTook::That,
        (Some(Some(r)), Some(p)) if r == p => HubTook::That,
        (None, Some(_)) => HubTook::Other(None),
        (Some(r), _) => HubTook::Other(Some(r.unwrap_or_else(|| "(not a path)".to_string()))),
    }
}

/// `box.projects {box: name}` to the hub on `socket`: `None` when no hub runs, else its result
/// once it took them, or why not (what to do).
async fn reload(socket: &Path, name: &str) -> Result<Option<Value>, String> {
    let mut c = match Client::connect(socket).await {
        Ok(c) => c,
        Err(e) if no_hub(&e) => return Ok(None),
        Err(e) => return Err(format!("the hub's socket did not answer: {e:#}")),
    };
    let hello = c
        .hello(json!({ "client": "cli" }))
        .await
        .map_err(|e| format!("the hub did not answer: {e:#}"))?;
    crate::check_workspace(&hello, socket).map_err(|e| format!("{e:#}"))?;
    match c.call("box.projects", json!({ "box": name })).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            let code = e.downcast_ref::<RpcError>().map(|r| r.code.as_str());
            Err(match code {
                Some(codes::CONFLICT) => format!(
                    "the running hub has other settings for box {name} than the files (its ssh \
                     destination, say), so it does not take its projects: restart it (`twr hub \
                     stop`, then `twr`)"
                ),
                Some(codes::NOT_FOUND) => format!(
                    "the running hub has no box {name}: restart it (`twr hub stop`, then `twr`) \
                     or add the box to it (`twr box add`)"
                ),
                _ => format!("the running hub did not take it: {e:#}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::config::BoxTransport;

    #[test]
    fn the_home_mapped_path_is_the_same_place_under_the_boxs_home() {
        let home = Path::new("/home/me");
        let m = |repo: &str, box_home: &str| home_mapped(Path::new(repo), Some(home), box_home);
        assert_eq!(
            m("/home/me/code/api", "/home/u"),
            Some("/home/u/code/api".into())
        );
        assert_eq!(m("/home/me/a/b/c", "/"), Some("/a/b/c".into()));
        assert_eq!(m("/home/me/x", "/home/u/"), Some("/home/u/x".into()));
        // Not under the home, the home itself, a relative box home, a control character.
        assert_eq!(m("/srv/api", "/home/u"), None);
        assert_eq!(m("/home/me", "/home/u"), None);
        assert_eq!(m("/home/meme/api", "/home/u"), None);
        assert_eq!(m("/home/me/code/api", "u"), None);
        assert_eq!(m("/home/me/a\nb", "/home/u"), None);
        // Quotes and the like are kept as they are (quoted where they are sent).
        assert_eq!(
            m("/home/me/it's $(x); a b/api", "/home/u"),
            Some("/home/u/it's $(x); a b/api".into())
        );
        assert_eq!(
            home_mapped(Path::new("/home/me/api"), None, "/home/u"),
            None
        );
        // Through a symbolic link to the home: canonical paths compared too.
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir_all(real.join("code/api")).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            home_mapped(&real.join("code/api"), Some(&link), "/b"),
            Some("/b/code/api".into())
        );
    }

    #[test]
    fn a_clone_goes_into_the_mapped_parent_or_code() {
        assert_eq!(
            clone_parent(Some("/h/code/org/api"), Some("/h")),
            Some("/h/code/org".into())
        );
        assert_eq!(clone_parent(None, Some("/h/")), Some("/h/code".into()));
        assert_eq!(clone_parent(None, None), None);
        assert_eq!(clone_parent(None, Some("rel")), None);
    }

    /// `--into <dir>` is the clone's own directory: its parent and its name, a plain folder
    /// name.
    #[test]
    fn into_names_the_clone_s_own_directory() {
        let split = |i: &str| split_into(i).map_err(|e| format!("{e:#}"));
        let pair = |p: &str, n: &str| Ok((p.to_string(), n.to_string()));
        assert_eq!(split("/srv/api-copy"), pair("/srv", "api-copy"));
        assert_eq!(split("/srv/api-copy/"), pair("/srv", "api-copy"));
        assert_eq!(split("/srv//api-copy//"), pair("/srv", "api-copy"));
        assert_eq!(split("/api"), pair("/", "api"));
        assert_eq!(split("/a b/x.y_z"), pair("/a b", "x.y_z"));
        for bad in [
            "/",
            "//",
            "/srv/..",
            "/srv/.",
            "/srv/.hidden",
            "/srv/-x",
            "/srv/a b",
        ] {
            let e = split(bad).unwrap_err();
            assert!(e.contains("not a folder name twr makes"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn the_clone_path_is_the_last_mark_line() {
        let out = b"Cloning into '/h/code/api'...\r\nremote: twr-clone-path /evil/api\r\n\
                    twr-clone-path /h/code/api\r\n";
        assert_eq!(clone_path(out, "api"), Some("/h/code/api".into()));
        let out = b"twr-clone-path /h/code/api\ntwr-clone-path /h/code/other\n";
        assert_eq!(clone_path(out, "api"), None, "the last one counts");
        assert_eq!(clone_path(b"twr-clone-path relative/api\n", "api"), None);
        assert_eq!(clone_path(b"twr-clone-path /a/api\x1b\n", "api"), None);
        assert_eq!(clone_path(b"nothing\n", "api"), None);
        // A post-checkout hook that printed without a newline: `--clone-repo` starts the mark
        // on a line of its own (unstarted, it would not be found).
        let out = b"Cloning into 'api'...\r\nhook ready\r\ntwr-clone-path /h/code/api\r\n";
        assert_eq!(clone_path(out, "api"), Some("/h/code/api".into()));
        assert_eq!(
            clone_path(b"hook readytwr-clone-path /h/code/api\r\n", "api"),
            None
        );
    }

    #[test]
    fn origins_twr_does_not_clone_are_refused_with_what_to_do() {
        let p = |origin: Option<&str>| {
            let raw = origin.map(str::to_string);
            let (origin, no_clone) = match &raw {
                None => (None, Some("it has no origin remote".to_string())),
                Some(r) => match RemoteUrl::parse_lenient(r) {
                    Ok(u) => (Some(u), RemoteUrl::parse(r).err()),
                    Err(e) => (None, Some(e)),
                },
            };
            HubProject {
                repo: PathBuf::from("/h/code/api"),
                folder: "api".into(),
                origin,
                no_clone,
            }
        };
        let ok = clone_url(&p(Some("git@github.com:o/api.git")), "gpu", "api").unwrap();
        assert_eq!(ok.redacted(), "git@github.com:o/api.git");
        for (origin, says) in [
            (
                Some("https://me:hunter2@github.com/o/api"),
                "password or a token",
            ),
            (Some("file:///srv/api.git"), "file://"),
            (Some("/srv/api.git"), "local path"),
            (Some("ext::sh -c x"), "space"),
            // A password holding `@` (and `:`, `/`, `#`): refused, no part of it shown.
            (
                Some("me:hunter2@SEC2@github.com:o/api"),
                "more than one `@`",
            ),
            (
                Some("https://u:hunter2:SEC2@SEC3@github.com/o/api"),
                "more than one `@`",
            ),
            (
                Some("ssh://git:hunter2/SEC2@SEC3@github.com/o/api"),
                "an `@` after the host",
            ),
            (Some("me:hunter2#SEC2@SEC3@github.com:o/api"), "query"),
            (None, "no origin"),
        ] {
            let e = format!("{:#}", clone_url(&p(origin), "gpu", "api").unwrap_err());
            assert!(e.contains(says), "{origin:?}: {e}");
            assert!(e.contains("Clone it on box gpu yourself"), "{e}");
            assert!(
                e.contains("`twr box project gpu api --path <path on the box>`"),
                "{e}"
            );
            assert!(!e.contains("hunter2") && !e.contains("SEC"), "{e}");
        }
    }

    #[test]
    fn box_paths_are_absolute_and_printable() {
        check_box_path("/srv/it's $(x); a b").unwrap();
        for bad in ["~/code/api", "code/api", "", "/a\nb"] {
            assert!(check_box_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_terminal_guard_needs_both() {
        require_terminal(true, true).unwrap();
        for (i, o) in [(false, true), (true, false), (false, false)] {
            let e = require_terminal(i, o).unwrap_err().to_string();
            assert!(e.contains("needs a terminal on stdin and stdout"), "{e}");
        }
    }

    /// Every operand of the box-side commands reaches `twr runner` intact, however it is
    /// written: the command line, run by the login shell and then `sh -c` as ssh runs it,
    /// gives back each argument as it was.
    #[test]
    fn box_side_operands_are_quoted() {
        let side = BoxSide {
            name: "gpu".into(),
            data: "/h/it's a $(touch x); `y`".into(),
            id: "0123456789abcdef".into(),
            home: Some("/h".into()),
        };
        let d = tempfile::tempdir().unwrap();
        let args = vec![
            "--find-repo".to_string(),
            "--name=api".to_string(),
            "--hint=/h/it's; $HOME \"q\" $(touch pwned) `touch pwned2` a\\b".to_string(),
        ];
        let script = side.runner(&args).replace("exec ", "printf '%s\\n' ");
        let remote = ssh_box::sh_c(&script);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&remote)
            .current_dir(d.path())
            .output()
            .unwrap();
        let got: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let mut want = vec![
            ssh_box::build_path(&side.data, &side.id),
            "runner".to_string(),
        ];
        want.extend(args);
        assert_eq!(got, want);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "ran");
        // The clone's, with a terminal: the same words after `--`.
        let url = RemoteUrl::parse("https://h/o/api.git").unwrap();
        let a = clone_args("me@gpu", &side, &url, "/h/a b; $(x)", "api").unwrap();
        assert!(a.iter().any(|w| w == "-t"), "{a:?}");
        let words = [
            "--clone-repo".to_string(),
            "--url=https://h/o/api.git".to_string(),
            "--parent=/h/a b; $(x)".to_string(),
            "--name=api".to_string(),
        ];
        assert_eq!(a.last().unwrap(), &ssh_box::sh_c(&side.runner(&words)));
        let script = side.runner(&words).replacen("exec ", "printf '%s\\n' ", 1);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(ssh_box::sh_c(&script))
            .current_dir(d.path())
            .output()
            .unwrap();
        let got = String::from_utf8(out.stdout).unwrap();
        assert!(
            got.ends_with(
                "runner\n--clone-repo\n--url=https://h/o/api.git\n--parent=/h/a b; $(x)\n\
                 --name=api\n"
            ),
            "{got}"
        );
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "ran");
    }

    /// The entry is written only while the box and the entry are what the command started
    /// from: a concurrent change to either is refused and the file left as it is; other
    /// entries and the entry's other keys are kept.
    #[test]
    fn a_conflicting_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config);
        std::fs::write(&config, "").unwrap();
        let ssh = |dest: &str| BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(dest.into()),
            ..BoxConfig::default()
        };
        let save = |c: &BoxConfig| {
            let mut b = BoxesFile::default();
            b.boxes.insert("gpu".into(), c.clone());
            b.save(&file).unwrap();
        };
        let mut start = ssh("me@gpu");
        start.projects.insert(
            "web".into(),
            BoxProjectConfig {
                repo: "/h/web".into(),
                runtime: None,
            },
        );
        save(&start);
        let entry = |start: &BoxConfig| Entry {
            config_file: &config,
            file: &file,
            box_name: "gpu",
            project: "api",
            start: start.clone(),
            in_file: true,
        };
        write_entry(&entry(&start), Some("/h/code/api")).unwrap();
        let now = BoxesFile::load(&file).unwrap().boxes["gpu"].clone();
        assert_eq!(now.projects["api"].repo, PathBuf::from("/h/code/api"));
        assert_eq!(now.projects["web"].repo, PathBuf::from("/h/web"));
        // Started before that write (no entry then): refused, the file as it is.
        let e = write_entry(&entry(&start), Some("/h/other")).unwrap_err();
        assert!(format!("{e:#}").contains("changed in"), "{e:#}");
        assert!(format!("{e:#}").contains("/h/code/api now"), "{e:#}");
        assert_eq!(BoxesFile::load(&file).unwrap().boxes["gpu"], now);
        // A removal started from it is refused the same way.
        assert!(write_entry(&entry(&start), None).is_err());
        // The box's destination changed meanwhile: refused.
        let mut moved = now.clone();
        moved.ssh = Some("me@other".into());
        save(&moved);
        let e = write_entry(&entry(&now), Some("/h/x")).unwrap_err();
        assert!(format!("{e:#}").contains("ssh destination"), "{e:#}");
        assert_eq!(BoxesFile::load(&file).unwrap().boxes["gpu"], moved);
        // Its build changed (`twr box install`): no conflict, and the build stays.
        let mut built = now.clone();
        built.build = Some("0123456789abcdef".into());
        save(&built);
        // A runtime of the entry is kept.
        let mut with_runtime = built.clone();
        with_runtime.projects.get_mut("api").unwrap().runtime = Some("codex".into());
        save(&with_runtime);
        write_entry(&entry(&with_runtime), Some("/h/code/api2")).unwrap();
        let after = BoxesFile::load(&file).unwrap().boxes["gpu"].clone();
        assert_eq!(after.projects["api"].repo, PathBuf::from("/h/code/api2"));
        assert_eq!(after.projects["api"].runtime.as_deref(), Some("codex"));
        assert_eq!(after.build.as_deref(), Some("0123456789abcdef"));
        // Removed.
        write_entry(&entry(&after), None).unwrap();
        let gone = BoxesFile::load(&file).unwrap().boxes["gpu"].clone();
        assert!(!gone.projects.contains_key("api"));
        assert!(gone.projects.contains_key("web"));
        // The box gone from the file meanwhile.
        BoxesFile::default().save(&file).unwrap();
        let e = write_entry(&entry(&gone), Some("/h/x")).unwrap_err();
        assert!(format!("{e:#}").contains("was removed"), "{e:#}");
    }

    /// The box as the command's config was loaded and as its file has it when the command
    /// takes its snapshot: a destination changed in between is refused before any ssh (the
    /// snapshot is what both the ssh and the write use); a change to the box's projects or
    /// build alone is not, and the snapshot has it.
    #[test]
    fn the_snapshot_is_refused_when_the_box_moved_since_the_config_was_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config);
        let ssh = |dest: &str| BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(dest.into()),
            ..BoxConfig::default()
        };
        let save = |c: &BoxConfig| {
            let mut b = BoxesFile::default();
            b.boxes.insert("gpu".into(), c.clone());
            b.save(&file).unwrap();
        };
        std::fs::write(&config, "").unwrap();
        let loaded = ssh("me@a");
        save(&loaded);
        let e = Entry::new(&config, &file, "gpu", "api", &loaded).unwrap();
        assert!(e.in_file);
        assert_eq!(e.start.ssh.as_deref(), Some("me@a"));
        // Moved to another destination after the config was loaded: refused.
        save(&ssh("me@b"));
        let err = format!(
            "{:#}",
            Entry::new(&config, &file, "gpu", "api", &loaded)
                .err()
                .unwrap()
        );
        assert!(err.contains("changed in"), "{err}");
        assert!(err.contains("nothing was changed"), "{err}");
        // A project recorded meanwhile, a build installed: the snapshot is the file's.
        let mut more = loaded.clone();
        more.build = Some("0123456789abcdef".into());
        more.projects.insert(
            "api".into(),
            BoxProjectConfig {
                repo: "/h/api".into(),
                runtime: None,
            },
        );
        save(&more);
        let e = Entry::new(&config, &file, "gpu", "api", &loaded).unwrap();
        assert_eq!(e.recorded(), Some(Path::new("/h/api")));
        assert_eq!(e.start, more);
        // Gone from boxes.toml: refused; in config.toml instead (as loaded): that one.
        BoxesFile::default().save(&file).unwrap();
        let err = format!(
            "{:#}",
            Entry::new(&config, &file, "gpu", "api", &loaded)
                .err()
                .unwrap()
        );
        assert!(err.contains("was removed"), "{err}");
        std::fs::write(
            &config,
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@a\"\n",
        )
        .unwrap();
        let e = Entry::new(&config, &file, "gpu", "api", &loaded).unwrap();
        assert!(!e.in_file);
        std::fs::write(
            &config,
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@b\"\n",
        )
        .unwrap();
        assert!(Entry::new(&config, &file, "gpu", "api", &loaded).is_err());
    }

    #[test]
    fn candidates_are_listed_with_their_origin() {
        let c = Candidate {
            path: "/h/code/api".into(),
            toplevel: true,
            origin: Some("https://***@github.com/o/api".into()),
            origin_unread: false,
            matches_origin: true,
            reason: Reason::Hint,
        };
        assert_eq!(
            candidate_line(&c),
            "  /h/code/api (origin https://***@github.com/o/api, the project's; where it is \
             under your home here)"
        );
        let c = Candidate {
            origin: None,
            toplevel: false,
            matches_origin: false,
            reason: Reason::Name,
            ..c
        };
        assert_eq!(
            candidate_line(&c),
            "  /h/code/api (no origin; inside a repository, not its top level)"
        );
        let c = Candidate {
            origin_unread: true,
            toplevel: true,
            ..c
        };
        assert_eq!(
            candidate_line(&c),
            "  /h/code/api (its origin could not be read)"
        );
        assert_eq!(toml_key("api"), "api");
        assert_eq!(toml_key("a.b"), "\"a.b\"");
    }

    /// What the box says is shown as twr shows it, whatever build of twr said it: an origin
    /// holding a password redacted, a long one cut, control characters escaped (in the path
    /// too).
    #[test]
    fn what_the_box_says_is_redacted_and_cleaned() {
        let c = Candidate {
            path: "/h/code/a\u{1b}]0;x\u{7}pi".into(),
            toplevel: true,
            origin: Some("https://me:hunter2@github.com/o/\u{1b}[2Japi".into()),
            origin_unread: false,
            matches_origin: false,
            reason: Reason::Name,
        };
        let line = candidate_line(&c);
        assert!(!line.contains("hunter2"), "{line}");
        assert!(!line.chars().any(|ch| ch.is_control()), "{line:?}");
        assert!(line.contains("https://***@github.com/o/"), "{line}");
        let long = Candidate {
            origin: Some(format!("https://h/{}", "a".repeat(5000))),
            ..c
        };
        let line = candidate_line(&long);
        assert!(
            line.contains("aaa...") && line.len() < 1200,
            "{}",
            line.len()
        );
    }

    /// "Recorded and active" only when the hub's repository for the project is the one written
    /// (or none, removed).
    #[test]
    fn the_hub_took_it_only_when_its_repository_is_the_one_written() {
        let r = json!({"box": "gpu", "projects": ["api"], "repos": {"api": "/h/api"}});
        assert_eq!(hub_took(&r, "api", Some("/h/api")), HubTook::That);
        assert_eq!(
            hub_took(&r, "api", Some("/h/other")),
            HubTook::Other(Some("/h/api".into()))
        );
        assert_eq!(
            hub_took(&r, "api", None),
            HubTook::Other(Some("/h/api".into()))
        );
        assert_eq!(hub_took(&r, "web", None), HubTook::That);
        assert_eq!(hub_took(&r, "web", Some("/h/web")), HubTook::Other(None));
        let older = json!({"box": "gpu", "projects": ["api"]});
        assert_eq!(hub_took(&older, "api", Some("/h/api")), HubTook::Unknown);
    }

    /// Why a search is incomplete: no home (it could not run), its deadline, or a limit.
    #[test]
    fn an_incomplete_search_says_why() {
        let r = |home: Option<&str>, timed_out: bool| FindReport {
            home: home.map(str::to_string),
            complete: false,
            git_missing: false,
            timed_out,
            candidates: Vec::new(),
        };
        assert!(incomplete_why("gpu", &r(None, false))
            .contains("could not run: box gpu has no absolute HOME"));
        assert!(incomplete_why("gpu", &r(Some("/h"), true)).contains("stopped after 90 s"));
        assert!(incomplete_why("gpu", &r(Some("/h"), false)).contains("too many folders"));
        // A box without git: refused, said so.
        refuse_without_git("gpu", &r(Some("/h"), false)).unwrap();
        let e = refuse_without_git(
            "gpu",
            &FindReport {
                git_missing: true,
                ..r(Some("/h"), false)
            },
        )
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("git is not installed on box gpu"),
            "{e:#}"
        );
    }
}

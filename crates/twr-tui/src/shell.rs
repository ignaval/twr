//! `!` commands: a line that starts with `!` runs as a shell command of the user's, by the hub
//! (`shell.run`, `twr_hub::hub::shell`), in a dive in the thread's worktree, in the
//! orchestrator view in its folder (`!cd` moves it). `!!` also tells the thread the command
//! and its output when it ends. A line that should be a message starting with `!` starts with
//! `\!`.
//!
//! The command is a job of its thread: its `task.updated` events make a [`Entry::Shell`] in
//! the transcript, in every TUI and in the history. While it runs the TUI reads the end of its
//! log ([`SHELL_POLL`]), and once more when it ends. Ctrl-C stops the newest command this TUI
//! started on the main pane's thread (`job.cancel`), and only then quits.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::app::{Action, AppState, Entry};

/// How often the output of a running command is read.
pub const SHELL_POLL: Duration = Duration::from_millis(500);

/// How many lines of its log a read takes (at most 64 KiB, the hub's cap).
pub const SHELL_READ_LINES: usize = 400;

/// A read on its way longer than this is taken as lost.
const SHELL_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A command this TUI started ([`AppState::shell_jobs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// Its job.
    pub job: String,
    /// The thread it runs for.
    pub thread: String,
    /// The command line.
    pub command: String,
    /// Ctrl-C asked to stop it.
    pub stopping: bool,
}

/// What the input box says while its line is a `!` command ([`AppState::shell_mode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    /// `!!`: the output goes to the thread too.
    pub share: bool,
    /// Where it runs, as the box's title says it.
    pub place: String,
    /// Who gets the output with `!!`.
    pub target: String,
}

/// `path` with the home directory as `~`.
pub fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && home != "/" => match path.strip_prefix(&home) {
            Some("") => "~".to_string(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// The directory `cd` names when `command` is only a `cd` (`cd`, `cd dir`, a quoted or plain
/// word): `Some(None)` for a bare `cd`; `None` for anything else (it runs as typed).
fn cd_target(command: &str) -> Option<Option<String>> {
    let rest = command.strip_prefix("cd")?;
    if rest.is_empty() {
        return Some(None);
    }
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let arg = rest.trim();
    if arg.is_empty() {
        return Some(None);
    }
    // One word, nothing the shell would read as more than a path.
    let unquoted = arg
        .strip_prefix('\'')
        .and_then(|a| a.strip_suffix('\''))
        .or_else(|| arg.strip_prefix('"').and_then(|a| a.strip_suffix('"')));
    match unquoted {
        Some(a) if !a.contains(['\'', '"', '$', '`', '\\']) => Some(Some(a.to_string())),
        Some(_) => None,
        None if arg.contains(|c: char| c.is_whitespace() || ";&|<>$`'\"\\(){}*?".contains(c)) => {
            None
        }
        None => Some(Some(arg.to_string())),
    }
}

impl AppState {
    /// What the input box says while the line is a `!` command (`None`: it isn't one).
    pub fn shell_mode(&self) -> Option<Mode> {
        let line = self.view.input.as_str().trim_start();
        if !line.starts_with('!') {
            return None;
        }
        let share = line.starts_with("!!");
        Some(
            match self.view.dive.as_ref().and_then(|d| self.registry.get(d)) {
                Some(t) => Mode {
                    share,
                    place: format!("{}'s worktree", t.name),
                    target: t.name.clone(),
                },
                None => Mode {
                    share,
                    place: self
                        .shell_cwd
                        .as_deref()
                        .map_or_else(|| "the orchestrator's folder".to_string(), tilde),
                    target: "the orchestrator".to_string(),
                },
            },
        )
    }

    /// Enter on a line that starts with `!` (`text`, trimmed): the command to run, or the
    /// folder to move to (`!cd`), or what the status bar says instead.
    pub(crate) fn shell_line(&mut self, text: &str) -> Vec<Action> {
        let share = text.starts_with("!!");
        let command = text
            .strip_prefix(if share { "!!" } else { "!" })
            .unwrap_or_default()
            .trim()
            .to_string();
        if command.is_empty() {
            self.view.status =
                Some("type a command after the `!` (`!!` sends its output too)".into());
            return Vec::new();
        }
        let dive = self.view.dive.clone();
        // A `!cd` on its way: where this would run is not known yet.
        if dive.is_none() && self.shell_cd_asked {
            self.view.status =
                Some("the folder is still changing (`!cd`): press Enter again in a moment".into());
            return Vec::new();
        }
        if let Some(dir) = cd_target(&command).filter(|_| !share) {
            if let Some(t) = dive.as_ref().and_then(|d| self.registry.get(d)) {
                self.view.status = Some(format!(
                    "a command for {} runs in its worktree: `!cd dir && ...` runs one in another \
                     folder",
                    t.name
                ));
                return Vec::new();
            }
            self.shell_cd_asked = true;
            self.shell_cds.push_back(true);
            // `!cd` alone: back to where commands start, the orchestrator's folder.
            let cwd = dir.as_ref().and(self.shell_cwd.clone());
            return vec![Action::ShellCd { cwd, dir }];
        }
        let cwd = if dive.is_none() {
            self.shell_cwd.clone()
        } else {
            None
        };
        self.shell_launch_seq += 1;
        let launch = self.shell_launch_seq;
        self.shell_launching.push_back(Launch {
            id: launch,
            thread: dive.clone().or_else(|| self.main_thread()),
            command: command.clone(),
            cancel: false,
        });
        vec![Action::ShellRun {
            thread: dive,
            command,
            cwd,
            share,
            launch,
        }]
    }

    /// The orchestrator view's folder, when it is not known yet and the line is a `!`
    /// command: asked once (`shell.cd`), for the box's title.
    pub fn shell_cwd_due(&mut self) -> bool {
        if self.shell_cwd.is_some()
            || self.shell_cwd_asked
            || self.view.dive.is_some()
            || !self.view.input.as_str().trim_start().starts_with('!')
        {
            return false;
        }
        self.shell_cwd_asked = true;
        self.shell_cds.push_back(false);
        true
    }

    /// The commands of the main pane's thread whose output is due to be read now: those
    /// running, every [`SHELL_POLL`], and each once more after it ended, or whose recorded
    /// output was cut; not while a read of it is on its way. Each is then on its way.
    pub fn shell_reads_due(&mut self, now: Instant) -> Vec<String> {
        let wanted: Vec<(String, bool)> = self
            .transcript
            .as_ref()
            .map(|t| {
                t.entries
                    .iter()
                    .filter_map(|e| match e {
                        Entry::Shell {
                            job, running, read, ..
                        } if *running || !*read => Some((job.clone(), *running)),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut due = Vec::new();
        for (job, running) in wanted {
            let r = self.shell_reads.entry(job.clone()).or_default();
            if r.asked
                .is_some_and(|a| now.saturating_duration_since(a) < SHELL_READ_TIMEOUT)
            {
                continue;
            }
            // A finished one is read once.
            if !running && r.done {
                continue;
            }
            if running
                && r.last
                    .is_some_and(|l| now.saturating_duration_since(l) < SHELL_POLL)
            {
                continue;
            }
            r.asked = Some(now);
            r.asked_running = running;
            due.push(job);
        }
        due
    }

    /// When the next read of a running command's output is due ([`AppState::shell_reads_due`]).
    pub fn shell_deadline(&self, now: Instant) -> Option<Instant> {
        let t = self.transcript.as_ref()?;
        t.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Shell {
                    job, running, read, ..
                } if *running || !*read => {
                    let r = self.shell_reads.get(job);
                    Some(match r {
                        Some(r) if r.asked.is_some() => {
                            r.asked.map_or(now, |a| a + SHELL_READ_TIMEOUT)
                        }
                        Some(r) if *running => r.last.map_or(now, |l| l + SHELL_POLL),
                        Some(r) if r.done => return None,
                        _ => now,
                    })
                }
                _ => None,
            })
            .min()
    }

    /// What `job.log` returned for a command (`failed`: the read failed, as for a log that is
    /// gone): its output, in the transcript.
    pub(crate) fn shell_log(&mut self, result: &Value, now: Instant) {
        let Some(job) = result["job"].as_str().map(str::to_string) else {
            return;
        };
        // Not on the main pane any more (another thread shown since): the read is over, and
        // the command is read again when its thread shows.
        let here = self.transcript.as_ref().is_some_and(|t| {
            t.entries
                .iter()
                .any(|e| matches!(e, Entry::Shell { job: j, .. } if *j == job))
        });
        if !here {
            if let Some(r) = self.shell_reads.get_mut(&job) {
                r.asked = None;
            }
            return;
        }
        let running = self.transcript.as_ref().is_some_and(|t| {
            t.entries
                .iter()
                .any(|e| matches!(e, Entry::Shell { job: j, running: true, .. } if *j == job))
        });
        let r = self.shell_reads.entry(job.clone()).or_default();
        r.asked = None;
        r.last = Some(now);
        // Asked while it ran, answered after its end: older than what its end recorded. A
        // read after the end follows.
        if r.asked_running && !running {
            r.asked_running = false;
            return;
        }
        if !running {
            r.done = true;
        }
        let text = result["text"]
            .as_str()
            .map(|t| twr_core::shell::plain_output(t).trim_end().to_string());
        if let Some(t) = self.transcript.as_mut() {
            for e in t.entries.iter_mut() {
                if let Entry::Shell {
                    job: j,
                    output,
                    read,
                    running,
                    ..
                } = e
                {
                    if *j != job {
                        continue;
                    }
                    if let Some(text) = &text {
                        *output = text.clone();
                    }
                    // Read after its end: what it shows is whole.
                    if !*running {
                        *read = true;
                    }
                }
            }
        }
    }

    /// What `shell.run` returned: the command is this TUI's (Ctrl-C stops it).
    pub(crate) fn shell_started(&mut self, result: &Value) {
        // The launch this answers: the oldest on its way (one connection answers in order).
        let launch = self.shell_launching.pop_front();
        let (Some(job), Some(thread)) = (result["job"].as_str(), result["thread"].as_str()) else {
            return;
        };
        // Ended already (its end came before this answer): nothing to stop.
        if self.shell_done.iter().any(|d| d == job) {
            return;
        }
        // Ctrl-C came before this answer: its stop went right after it (ShellStopLaunch).
        if launch.is_some_and(|l| l.cancel) {
            self.shell_jobs.push(Started {
                job: job.to_string(),
                thread: thread.to_string(),
                command: result["name"].as_str().unwrap_or_default().to_string(),
                stopping: true,
            });
            return;
        }
        // Its `■ stop` was clicked before this answer: it is being stopped already.
        if self.shell_jobs.iter().any(|s| s.job == job) {
            return;
        }

        if let Some(cwd) = result["cwd"].as_str().filter(|_| {
            self.registry
                .get(thread)
                .is_some_and(|t| t.is_orchestrator())
        }) {
            self.shell_cwd = Some(cwd.to_string());
        }
        self.shell_jobs.push(Started {
            job: job.to_string(),
            thread: thread.to_string(),
            command: result["name"].as_str().unwrap_or_default().to_string(),
            stopping: false,
        });
        self.view.status = Some("running · Ctrl-C stops it".to_string());
    }

    /// What `shell.cd` returned: the orchestrator view's folder from now on.
    pub(crate) fn shell_moved(&mut self, result: &Value) {
        // Which call this answers: the oldest on its way.
        let users = self.shell_cds.pop_front().unwrap_or(false);
        let more = self.shell_cds.iter().any(|u| *u);
        if users {
            // Another `!cd` after it still on its way: commands wait for that one too.
            self.shell_cd_asked = more;
        }
        // A failed `!cd` (its error is in the status bar): commands run where they did.
        if result["failed"] == true {
            return;
        }
        let Some(cwd) = result["cwd"].as_str() else {
            return;
        };
        // The folder's name, asked before a `!cd` that hasn't answered yet: that one says.
        if !users && (more || self.shell_cwd.is_some()) {
            return;
        }
        self.shell_cwd = Some(cwd.to_string());
        if users {
            self.view.status = Some(format!("`!` commands run in {}", tilde(cwd)));
        }
    }

    /// A command's job ended (`task.updated`, not running): it is no longer this TUI's to stop.
    pub(crate) fn shell_ended(&mut self, job: &str) {
        self.shell_done.push_back(job.to_string());
        if self.shell_done.len() > 32 {
            self.shell_done.pop_front();
        }
        let Some(i) = self.shell_jobs.iter().position(|s| s.job == job) else {
            return;
        };
        let s = self.shell_jobs.remove(i);
        // The status said it was being stopped: now it is.
        let stopping = format!("stopping `{}`", s.command);
        if self
            .view
            .status
            .as_deref()
            .is_some_and(|m| m.starts_with(&stopping))
        {
            self.view.status = Some(format!("stopped `{}`", s.command));
        }
    }

    /// Ctrl-C: stops the newest command this TUI started on the main pane's thread that still
    /// runs and isn't being stopped; with none, quits.
    pub(crate) fn ctrl_c(&mut self) -> Vec<Action> {
        let main = self.main_thread();
        // A stop asked for before the command's job is known: quitting now would leave it
        // running, its stop never sent. One more Ctrl-C says so; the next quits.
        let stop_pending = self.shell_launching.iter().any(|l| l.cancel);
        if !stop_pending {
            self.shell_quit_armed = false;
        }
        let fresh = self
            .shell_launching
            .iter()
            .any(|l| l.thread == main && !l.cancel)
            || self
                .shell_jobs
                .iter()
                .any(|s| Some(&s.thread) == main.as_ref() && !s.stopping);
        if stop_pending && !fresh {
            if std::mem::replace(&mut self.shell_quit_armed, true) {
                return vec![Action::Quit];
            }
            self.view.status = Some(
                "a `!` command's stop is still on its way: Ctrl-C again quits without \
                 waiting for it"
                    .into(),
            );
            return Vec::new();
        }
        // The newest is one sent and not answered yet: it stops as soon as its job is known.
        if let Some(l) = self
            .shell_launching
            .iter_mut()
            .rev()
            .find(|l| l.thread == main && !l.cancel)
        {
            l.cancel = true;
            self.view.status = Some(format!("stopping `{}` as it starts", l.command));
            return vec![Action::ShellStopLaunch { launch: l.id }];
        }
        let Some(s) = self
            .shell_jobs
            .iter_mut()
            .rev()
            .find(|s| Some(&s.thread) == main.as_ref() && !s.stopping)
        else {
            return vec![Action::Quit];
        };
        s.stopping = true;
        let (job, command) = (s.job.clone(), s.command.clone());
        self.view.status = Some(format!("stopping `{command}` · Ctrl-C again quits"));
        vec![Action::JobCancel { job }]
    }

    /// What `job.cancel` answered (`{failed, job, error}` when it failed): a stop that failed
    /// is no longer pending, and the status bar says so.
    pub(crate) fn shell_cancel_result(&mut self, result: &Value) {
        if result["failed"] != true {
            return;
        }
        let Some(job) = result["job"].as_str() else {
            return;
        };
        if let Some(s) = self.shell_jobs.iter_mut().find(|s| s.job == job) {
            s.stopping = false;
            let why = result["error"].as_str().unwrap_or("no answer");
            self.view.status = Some(format!(
                "could not stop `{}` ({why}): Ctrl-C or ■ stop tries again",
                s.command
            ));
        }
    }

    /// A click on a running command's `■ stop`: stops it, whoever started it.
    pub(crate) fn stop_shell(&mut self, job: &str) -> Vec<Action> {
        match self.shell_jobs.iter_mut().find(|s| s.job == job) {
            Some(s) => s.stopping = true,
            // Not this TUI's, or its answer not in yet: followed from now on.
            None => self.shell_jobs.push(Started {
                job: job.to_string(),
                thread: self.main_thread().unwrap_or_default(),
                command: String::new(),
                stopping: true,
            }),
        }
        self.view.status = Some("stopping the command".into());
        vec![Action::JobCancel {
            job: job.to_string(),
        }]
    }
}

/// A `!` command sent and not answered yet ([`AppState::shell_launching`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Its number ([`Action::ShellRun`]'s `launch`).
    pub id: u64,
    /// The thread it runs for (the orchestrator's: the main pane's then).
    pub thread: Option<String>,
    /// The command line.
    pub command: String,
    /// Ctrl-C asked to stop it.
    pub cancel: bool,
}

/// The `!` commands' state a view keeps across a switch to another workspace
/// ([`crate::workspace::SavedView`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    cwd: Option<String>,
    cwd_asked: bool,
    cd_asked: bool,
    cds: std::collections::VecDeque<bool>,
    jobs: Vec<Started>,
    done: std::collections::VecDeque<String>,
    launching: std::collections::VecDeque<Launch>,
    launch_seq: u64,
}

impl AppState {
    /// The `!` commands' state, for the view kept across a switch.
    pub(crate) fn take_shell(&mut self) -> Saved {
        Saved {
            cwd: self.shell_cwd.take(),
            cwd_asked: std::mem::take(&mut self.shell_cwd_asked),
            cd_asked: std::mem::take(&mut self.shell_cd_asked),
            cds: std::mem::take(&mut self.shell_cds),
            jobs: std::mem::take(&mut self.shell_jobs),
            done: std::mem::take(&mut self.shell_done),
            launching: std::mem::take(&mut self.shell_launching),
            launch_seq: self.shell_launch_seq,
        }
    }

    /// Puts back the `!` commands' state of a view kept across a switch.
    pub(crate) fn restore_shell(&mut self, s: Saved) {
        self.shell_cwd = s.cwd;
        self.shell_cwd_asked = s.cwd_asked;
        self.shell_cd_asked = s.cd_asked;
        self.shell_cds = s.cds;
        self.shell_jobs = s.jobs;
        self.shell_done = s.done;
        self.shell_launching = s.launching;
        self.shell_launch_seq = self.shell_launch_seq.max(s.launch_seq);
    }
}

/// How the output of one command is read ([`AppState::shell_reads`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reads {
    /// A read is on its way, since then.
    pub asked: Option<Instant>,
    /// The last read answered then.
    pub last: Option<Instant>,
    /// Read after it ended: nothing more to read.
    pub done: bool,
    /// The read on its way was asked while the command ran.
    pub asked_running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_cd_moves_the_folder() {
        assert_eq!(cd_target("cd"), Some(None));
        assert_eq!(cd_target("cd   "), Some(None));
        assert_eq!(cd_target("cd src"), Some(Some("src".into())));
        assert_eq!(cd_target("cd ~/code/api"), Some(Some("~/code/api".into())));
        assert_eq!(cd_target("cd 'my dir'"), Some(Some("my dir".into())));
        assert_eq!(cd_target("cd \"my dir\""), Some(Some("my dir".into())));
        // Anything more runs as typed (in a shell of its own: it moves no folder).
        assert_eq!(cd_target("cd src && ls"), None);
        assert_eq!(cd_target("cd $HOME"), None);
        assert_eq!(cd_target("cd a b"), None);
        assert_eq!(cd_target("cdx"), None);
        assert_eq!(cd_target("ls"), None);
    }
}

//! `twr loop`: review and implementation loops from the command line (docs/design/loops.md,
//! docs/usage.md "Loops"). `review` and `implement` ask the hub for a loop (`loop.request`);
//! the hub alone decides whether it starts on the saved settings or raises the thread's setup
//! card, which this command then shows and, on a terminal, answers as the user says
//! (`attention.respond`, at the revision shown) or changes in the editor (`loop.setup.update`).
//! `twr thread new … --loop implement` does the same for a new thread (`thread.new {loop}`,
//! [`after_new_thread`]). `list`, `show` and `report` read (`loop.list`, `loop.get`,
//! `loop.report`); `pause`, `resume`, `stop` and `skip` control (`loop.control`); `settings`
//! shows, edits, sets and resets the saved settings (`loop.settings.*`).
//!
//! There is no flag that starts a first loop without its card: the card is how the user
//! chooses the models a loop runs on, and only an answer to it, or settings the user saved,
//! start one (the hub enforces it).
//!
//! The settings editor is a small commented TOML document ([`render_doc`], [`parse_doc`]) in
//! `$VISUAL`, else `$EDITOR`, else `vi`, written to a private temporary copy as `twr context
//! edit` does: nothing is lost when the save fails (the copy is kept and its path printed).

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};
use twr_core::loops::config::{
    check_profile_name, parse_implementer, ImplCommit, PlanCheck, ReviewCommit, ReviewerChoice,
    WorkersChoice,
};
use twr_core::loops::settings::LoopSettings;

use super::{
    ask_on_stderr, connect, consent, localize, refuse_agent_action, revisioned_id, str_of, table,
    workspace_hint, Consent, TempFile,
};

/// The setup card's option that starts the loop and saves the settings it shows.
const START: &str = "start and save these settings";

/// The setup card's option that starts nothing and saves nothing.
const CANCEL: &str = "cancel";

/// The option of a setup card that holds a task: send the task as your own message, no loop.
const RUN_WITHOUT: &str = twr_core::event::LOOP_SETUP_RUN_WITHOUT;

/// `twr loop review`'s exit code when it leaves the setup card open: the loop waits on you
/// (as `twr wait`'s 2).
const LEFT_OPEN: u8 = 2;

#[derive(Debug, Subcommand)]
pub(super) enum LoopCmd {
    /// Start a review loop on a thread's work: an adversarial reviewer (another model, run
    /// read-only) reviews it round after round, and the thread verifies and fixes or dismisses
    /// each finding, until the reviewer has nothing left. It starts at once on your saved loop
    /// settings and prints them. The first time, or with --setup, or when a saved setting no
    /// longer holds, the thread's settings card comes first: on a terminal you answer it here
    /// (1 start and save, 2 change… in your editor, 3 cancel); without one it stays open, this
    /// prints the `twr respond` line that starts it, and exits 2. No flag starts a first loop
    /// without that card.
    Review {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Show the settings card first, even when saved settings hold; starting from it saves
        /// what it shows.
        #[arg(long)]
        setup: bool,
        /// What to review: branch (the default when the thread has commits of its own),
        /// uncommitted (else), since:<ref> or last:<n> (also written "since <ref>" in quotes,
        /// or since=<ref>).
        #[arg(long, value_name = "SCOPE", value_parser = scope_arg)]
        scope: Option<String>,
        /// This run's profile: low, medium, high, or one of your [loops.profiles].
        #[arg(long, value_parser = profile_arg)]
        profile: Option<String>,
        /// This run's reviewer: runtime[:account]/model (codex/gpt-6.1-sol).
        #[arg(long, value_parser = reviewer_arg)]
        reviewer: Option<String>,
        /// This run's implementer: thread (its own model), or runtime[:account]/model[:effort]
        /// on the thread's own runtime.
        #[arg(long, value_parser = implementer_arg)]
        implementer: Option<String>,
        /// This run's workers, the implementer's subagents: model[:effort].
        #[arg(long, value_parser = workers_arg)]
        workers: Option<String>,
        /// This run's commit: match (commit the fixes if the thread commits its work) or none.
        #[arg(long, value_parser = commit_arg)]
        commit: Option<String>,
        /// Print `loop.request`'s answer as JSON (with the card, `loop.setup.get`'s, when it
        /// needs one); never asks.
        #[arg(long)]
        json: bool,
    },
    /// Start an implementation loop in a thread: it plans the task, has the plan reviewed when
    /// it touches a contract, implements it with the gates green, then runs the review loop on
    /// the result, and commits at the end. The task is the words after the thread (`-` alone
    /// reads it from stdin; --task-file from a file); without one, it is what the thread's
    /// conversation already settled. Changes already in the worktree are the loop's baseline:
    /// out of its scope and out of its commit. It starts at once on your saved loop settings;
    /// the first time, with --setup, or when a saved setting no longer holds, the thread's
    /// settings card comes first, as for `twr loop review`, with one more answer when it holds a
    /// task: run without a loop (the task goes to the thread as your own message). Without a
    /// terminal the card stays open, this prints the `twr respond` lines for it, and exits 2.
    Implement {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// The task, as words (`-` alone: read it from stdin). Put `--` before a task whose
        /// words start with `-`.
        #[arg(value_name = "TASK", conflicts_with = "task_file")]
        task: Vec<String>,
        /// Read the task from this file (for a long one).
        #[arg(long, value_name = "PATH")]
        task_file: Option<std::path::PathBuf>,
        /// Show the settings card first, even when saved settings hold; starting from it saves
        /// what it shows.
        #[arg(long)]
        setup: bool,
        /// This run's profile: low, medium, high, or one of your [loops.profiles].
        #[arg(long, value_parser = profile_arg)]
        profile: Option<String>,
        /// This run's reviewer: runtime[:account]/model (codex/gpt-6.1-sol).
        #[arg(long, value_parser = reviewer_arg)]
        reviewer: Option<String>,
        /// This run's implementer: thread (its own model), or runtime[:account]/model[:effort]
        /// on the thread's own runtime.
        #[arg(long, value_parser = implementer_arg)]
        implementer: Option<String>,
        /// This run's workers, the implementer's subagents: model[:effort].
        #[arg(long, value_parser = workers_arg)]
        workers: Option<String>,
        /// This run's commit: end (one commit at the end), per-round, or none.
        #[arg(long, value_parser = impl_commit_arg)]
        commit: Option<String>,
        /// This run's plan check: contracts (when the plan touches a contract), always or never.
        #[arg(long, value_parser = plan_check_arg)]
        plan_check: Option<String>,
        /// Print `loop.request`'s answer as JSON (with the card, `loop.setup.get`'s, when it
        /// needs one); never asks.
        #[arg(long)]
        json: bool,
    },
    /// List the running loops: thread, kind, profile, phase, round, ledger, what each waits on.
    #[command(alias = "ls")]
    List {
        /// Print the raw `loop.list` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a thread's loop (its running one, else its last): its line, the settings it runs
    /// on and where they came from, each phase, each round with its findings and their
    /// triage, the ledger, the transition, how it ended, its folder and report.
    Show {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Print the raw `loop.get` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pause a thread's loop: what runs now finishes, the next step waits for resume.
    Pause {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Print the raw `loop.control` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resume a paused or stuck loop (a stuck step is asked for again).
    Resume {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Print the raw `loop.control` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop a thread's loop: it ends `stopped`, and the thread and its work stay as they are.
    /// Asks y/N on a terminal; without one, only with --yes.
    Stop {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Stop it without asking.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print the raw `loop.control` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Skip to verification: the ladder ends after the round under way, and the sweep is
    /// skipped.
    Skip {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Print the raw `loop.control` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the report of a thread's loop (its running one's, else its last one's: its
    /// `report.md`), as the hub reads it (`loop.report`), so it works from any machine that
    /// reaches the hub. A long report is cut, with a line saying where the whole of it is.
    Report {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Print `loop.report`'s answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the saved loop settings that apply (a project's own, else the workspace's), where
    /// they come from, what the first settings card shows when nothing is saved, and the
    /// warnings and problems; --edit changes them in your editor ($VISUAL, $EDITOR, else vi)
    /// as a small TOML document; --reset forgets them (the next loop shows its card again).
    /// `twr loop settings set` changes named settings without an editor.
    #[command(args_conflicts_with_subcommands = true)]
    Settings {
        #[command(subcommand)]
        cmd: Option<LoopSettingsCmd>,
        /// One project's settings (it uses the workspace's until it has its own).
        #[arg(long)]
        project: Option<String>,
        /// Change them in your editor and save them (for --project: as that project's own).
        #[arg(long, conflicts_with = "reset")]
        edit: bool,
        /// Forget them: a project's own (it uses the workspace's again), or, without --project,
        /// every saved set. Asks y/N on a terminal; without one, only with --yes.
        #[arg(long)]
        reset: bool,
        /// With --reset: do it without asking.
        #[arg(long, short = 'y', requires = "reset")]
        yes: bool,
        /// Print the raw `loop.settings.get` JSON (with --edit or --reset: the answer of the
        /// change).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum LoopSettingsCmd {
    /// Change named settings and save them (`loop.settings.set`), the others as they apply now
    /// (or, before anything is saved, as the first card shows them). Saving them counts as
    /// your answer: later loops start on them without a card.
    Set {
        /// Save them as this project's own.
        #[arg(long)]
        project: Option<String>,
        /// A preset's implementer, workers and reviewer first: claude-codex (Claude Code
        /// implements, Codex reviews), claude, codex or grok.
        #[arg(long, value_parser = ["claude-codex", "claude", "codex", "grok"])]
        preset: Option<String>,
        /// The profile: low, medium, high, or one of your [loops.profiles].
        #[arg(long, value_parser = profile_arg)]
        profile: Option<String>,
        /// The implementer: thread, or runtime[:account]/model[:effort].
        #[arg(long, value_parser = implementer_arg)]
        implementer: Option<String>,
        /// The workers: model[:effort].
        #[arg(long, value_parser = workers_arg)]
        workers: Option<String>,
        /// The reviewer: runtime[:account]/model.
        #[arg(long, value_parser = reviewer_arg)]
        reviewer: Option<String>,
        /// What a review loop commits: match or none.
        #[arg(long, value_parser = commit_arg)]
        commit: Option<String>,
        /// What an implementation loop commits: end, per-round or none.
        #[arg(long, value_parser = impl_commit_arg)]
        implementation_commit: Option<String>,
        /// When an implementation loop checks its plan: profile (the profile's), contracts,
        /// always or never.
        #[arg(long, value_parser = ["profile", "contracts", "always", "never"])]
        plan_check: Option<String>,
        /// Who audits the sweep's rules: thread (the thread, with its own subagents) or runs
        /// (one read-only reviewer run per rule, up to `[loops] max_audit_runs` at once: one
        /// reviewer call per rule).
        #[arg(long, value_parser = ["thread", "runs"])]
        sweep: Option<String>,
        /// Print `loop.settings.set`'s answer as JSON.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------------------------
// Values, checked with the loop settings' own parsers
// ---------------------------------------------------------------------------------------------

fn scope_arg(s: &str) -> Result<String, String> {
    twr_core::slash::scope_value(s)
}

fn profile_arg(s: &str) -> Result<String, String> {
    check_profile_name(s.trim()).map(|()| s.trim().to_string())
}

fn reviewer_arg(s: &str) -> Result<String, String> {
    ReviewerChoice::parse(s).map(|r| r.to_string())
}

fn implementer_arg(s: &str) -> Result<String, String> {
    parse_implementer(s).map(|m| m.map_or_else(|| "thread".to_string(), |m| m.to_string()))
}

fn workers_arg(s: &str) -> Result<String, String> {
    WorkersChoice::parse(s).map(|w| w.to_string())
}

fn commit_arg(s: &str) -> Result<String, String> {
    ReviewCommit::parse(s).map(|c| c.as_str().to_string())
}

fn impl_commit_arg(s: &str) -> Result<String, String> {
    ImplCommit::parse(s).map(|c| c.as_str().to_string())
}

fn plan_check_arg(s: &str) -> Result<String, String> {
    PlanCheck::parse(s).map(|c| c.as_str().to_string())
}

/// The flags of `twr thread new … --loop implement`: the new thread runs its first prompt, the
/// task, as an implementation loop (`thread.new {loop}`).
#[derive(Debug, Default, clap::Args)]
pub(super) struct NewLoopArgs {
    /// Run the new thread as a loop: `implement` (an implementation loop whose task is the
    /// words after the project, or --task-file, or `-` for stdin).
    #[arg(long = "loop", value_name = "KIND", value_parser = ["implement"])]
    pub(super) kind: Option<String>,
    /// The task (with --loop implement): the thread's first prompt. `-` alone reads it from
    /// stdin. Put `--` before a task whose words start with `-`.
    #[arg(value_name = "TASK", requires = "kind", conflicts_with = "task_file")]
    pub(super) task: Vec<String>,
    /// Read the task from this file (with --loop implement).
    #[arg(long, value_name = "PATH", requires = "kind")]
    pub(super) task_file: Option<std::path::PathBuf>,
    /// Show the loop's settings card first, even when saved settings hold.
    #[arg(long, requires = "kind")]
    pub(super) setup: bool,
    /// The loop's profile for this run: low, medium, high, or one of your [loops.profiles].
    #[arg(long, value_parser = profile_arg, requires = "kind")]
    pub(super) profile: Option<String>,
    /// The loop's reviewer for this run: runtime[:account]/model.
    #[arg(long, value_parser = reviewer_arg, requires = "kind")]
    pub(super) reviewer: Option<String>,
    /// The loop's implementer for this run: thread, or runtime[:account]/model[:effort]. A
    /// named one is the runtime and model the thread is made on, unless --runtime or --model
    /// says otherwise.
    #[arg(long, value_parser = implementer_arg, requires = "kind")]
    pub(super) implementer: Option<String>,
    /// The loop's workers for this run: model[:effort].
    #[arg(long, value_parser = workers_arg, requires = "kind")]
    pub(super) workers: Option<String>,
    /// The loop's commit for this run: end, per-round or none.
    #[arg(long, value_parser = impl_commit_arg, requires = "kind")]
    pub(super) commit: Option<String>,
    /// The loop's plan check for this run: contracts, always or never.
    #[arg(long, value_parser = plan_check_arg, requires = "kind")]
    pub(super) plan_check: Option<String>,
    /// Print `thread.new`'s answer as JSON (with the loop's card when it needs one); never
    /// asks.
    #[arg(long)]
    pub(super) json: bool,
}

/// `thread.new`'s `loop` of `twr thread new … --loop implement`: `{kind, task, setup?,
/// overrides?}`, and whether the task was read from stdin. A new thread's loop needs a task.
pub(super) fn new_thread_loop_param(a: NewLoopArgs) -> anyhow::Result<(Value, bool)> {
    let kind = a.kind.unwrap_or_else(|| "implement".into());
    let (task, stdin) = read_task(&a.task, a.task_file.as_deref())?;
    let Some(task) = task else {
        bail!(
            "--loop {kind} needs the task: the words after the project (or --task-file, or `-` \
             to read it from stdin), the new thread's first prompt"
        );
    };
    let mut l = json!({ "kind": kind, "task": task });
    if a.setup {
        l["setup"] = json!(true);
    }
    if let Some(o) = overrides_of([
        ("profile", a.profile),
        ("reviewer", a.reviewer),
        ("implementer", a.implementer),
        ("workers", a.workers),
        ("commit", a.commit),
        ("plan_check", a.plan_check),
    ]) {
        l["overrides"] = o;
    }
    Ok((l, stdin))
}

/// This run's overrides from the flags given, as `loop.request` takes them.
fn overrides_of(pairs: [(&str, Option<String>); 6]) -> Option<Value> {
    let mut overrides = serde_json::Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            overrides.insert(k.into(), json!(v));
        }
    }
    (!overrides.is_empty()).then_some(Value::Object(overrides))
}

/// The task of an implementation loop from its words (joined by single spaces; `-` alone:
/// stdin) or `file`; `None` when there is none. The second value: whether stdin was read
/// (no terminal is left to answer a card on).
fn read_task(
    words: &[String],
    file: Option<&std::path::Path>,
) -> anyhow::Result<(Option<String>, bool)> {
    let (text, stdin) = match (words, file) {
        (_, Some(f)) => (
            std::fs::read_to_string(f)
                .with_context(|| format!("cannot read the task from {}", f.display()))?,
            false,
        ),
        ([one], None) if one == "-" => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .context("cannot read the task from stdin")?;
            (text, true)
        }
        (words, None) => (
            words
                .iter()
                .map(|w| w.trim())
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            false,
        ),
    };
    let text = text.trim();
    Ok(((!text.is_empty()).then(|| text.to_string()), stdin))
}

/// The hub's preset id of `--preset`'s word.
fn preset_id(word: &str) -> &str {
    match word {
        "claude-codex" => "claude-code+codex",
        "claude" => "claude-code",
        "grok" => "grok-build",
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// Runs `twr loop <cmd>` against the hub at `socket`.
pub(super) async fn run(cmd: LoopCmd, socket: &Path) -> anyhow::Result<ExitCode> {
    match cmd {
        LoopCmd::Review {
            thread,
            setup,
            scope,
            profile,
            reviewer,
            implementer,
            workers,
            commit,
            json,
        } => {
            refuse_agent_action("start a loop")?;
            let mut overrides = serde_json::Map::new();
            for (k, v) in [
                ("profile", profile),
                ("reviewer", reviewer),
                ("implementer", implementer),
                ("workers", workers),
                ("commit", commit),
            ] {
                if let Some(v) = v {
                    overrides.insert(k.into(), json!(v));
                }
            }
            let mut params = json!({ "thread": thread, "kind": "review" });
            if setup {
                params["setup"] = json!(true);
            }
            if let Some(s) = scope {
                params["scope"] = json!(s);
            }
            if !overrides.is_empty() {
                params["overrides"] = Value::Object(overrides);
            }
            let mut ask = ask_on_stderr;
            let consent = if json {
                Consent::NoTerminal
            } else {
                consent(false, &mut ask)
            };
            request(socket, params, json, consent).await
        }
        LoopCmd::Implement {
            thread,
            task,
            task_file,
            setup,
            profile,
            reviewer,
            implementer,
            workers,
            commit,
            plan_check,
            json,
        } => {
            refuse_agent_action("start a loop")?;
            let (task, stdin) = read_task(&task, task_file.as_deref())?;
            let mut params = json!({ "thread": thread, "kind": "implement" });
            if let Some(t) = task {
                params["task"] = json!(t);
            }
            if setup {
                params["setup"] = json!(true);
            }
            if let Some(o) = overrides_of([
                ("profile", profile),
                ("reviewer", reviewer),
                ("implementer", implementer),
                ("workers", workers),
                ("commit", commit),
                ("plan_check", plan_check),
            ]) {
                params["overrides"] = o;
            }
            let mut ask = ask_on_stderr;
            let consent = if json || stdin {
                Consent::NoTerminal
            } else {
                consent(false, &mut ask)
            };
            request(socket, params, json, consent).await
        }
        LoopCmd::List { json } => {
            let list = connect(socket)
                .await?
                .call("loop.list", Value::Null)
                .await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                out!("{}", list_text(&list));
            }
            Ok(ExitCode::SUCCESS)
        }
        LoopCmd::Show { thread, json } => {
            let detail = connect(socket)
                .await?
                .call("loop.get", json!({ "thread": thread }))
                .await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                outln!("{}", show_text(&detail));
            }
            Ok(ExitCode::SUCCESS)
        }
        LoopCmd::Pause { thread, json } => control(socket, &thread, "pause", json).await,
        LoopCmd::Resume { thread, json } => control(socket, &thread, "resume", json).await,
        LoopCmd::Skip { thread, json } => control(socket, &thread, "skip_to_verify", json).await,
        LoopCmd::Stop { thread, yes, json } => {
            refuse_agent_action("stop a loop")?;
            let mut ask = ask_on_stderr;
            match consent(yes, &mut ask) {
                Consent::Yes => {}
                Consent::Ask(ask) => {
                    let reply = ask(&format!("Stop {thread}'s loop? [y/N] "))?;
                    if !matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                        outln!("{thread}'s loop goes on");
                        return Ok(ExitCode::SUCCESS);
                    }
                }
                Consent::NoTerminal => bail!(
                    "not stopping {thread}'s loop without confirmation (no terminal to ask on): \
                     pass --yes"
                ),
            }
            control(socket, &thread, "stop", json).await
        }
        LoopCmd::Report { thread, json } => report(socket, &thread, json).await,
        LoopCmd::Settings {
            cmd:
                Some(LoopSettingsCmd::Set {
                    project,
                    preset,
                    profile,
                    implementer,
                    workers,
                    reviewer,
                    commit,
                    implementation_commit,
                    plan_check,
                    sweep,
                    json,
                }),
            ..
        } => {
            refuse_agent_action("save loop settings")?;
            let changes = Changes {
                preset,
                profile,
                implementer,
                workers,
                reviewer,
                commit,
                implementation_commit,
                plan_check,
                sweep,
            };
            settings_set(socket, project, changes, json).await?;
            Ok(ExitCode::SUCCESS)
        }
        LoopCmd::Settings {
            cmd: None,
            project,
            edit,
            reset,
            yes,
            json,
        } => {
            if reset {
                refuse_agent_action("reset loop settings")?;
                let mut ask = ask_on_stderr;
                settings_reset(socket, project, consent(yes, &mut ask), json).await?;
            } else if edit {
                refuse_agent_action("save loop settings")?;
                settings_edit(socket, project, json).await?;
            } else {
                let got = connect(socket)
                    .await?
                    .call("loop.settings.get", project_params(&project))
                    .await?;
                if json {
                    outln!("{}", serde_json::to_string_pretty(&got)?);
                } else {
                    outln!("{}", settings_text(&got, project.as_deref()));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn project_params(project: &Option<String>) -> Value {
    match project {
        Some(p) => json!({ "project": p }),
        None => json!({}),
    }
}

/// `twr loop review` and `twr loop implement`: `loop.request`, then the loop it started, or
/// its setup card ([`answer_card`]).
async fn request(
    socket: &Path,
    params: Value,
    json: bool,
    consent: Consent<'_>,
) -> anyhow::Result<ExitCode> {
    let mut c = connect(socket).await?;
    let thread = str_of(&params["thread"]).to_string();
    let r = c.call("loop.request", params).await?;
    if r["status"] == "started" {
        let detail = c.call("loop.get", json!({ "thread": thread })).await?;
        if json {
            let mut r = r;
            r["detail"] = detail;
            outln!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            outln!("{}", started_text(&detail));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let attn_id = str_of(&r["attn_id"]).to_string();
    let card = c
        .call("loop.setup.get", json!({ "attn_id": attn_id }))
        .await?;
    if json {
        let mut r = r;
        add_card(&mut r, card);
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(ExitCode::from(LEFT_OPEN));
    }
    answer_card(&mut c, socket, &thread, card, consent).await
}

/// `card` (`loop.setup.get`'s) into `r` (`--json`'s answer): `card`, the `respond` line that
/// starts the loop, and, on a card that holds a task, the `run_without` line.
fn add_card(r: &mut Value, card: Value) {
    r["respond"] = json!(respond_line(&card, START));
    if offers_run_without(&card) {
        r["run_without"] = json!(respond_line(&card, RUN_WITHOUT));
    }
    r["card"] = card;
}

/// Whether `card` has the option [`RUN_WITHOUT`] (it holds a task).
fn offers_run_without(card: &Value) -> bool {
    card["options"]
        .as_array()
        .is_some_and(|o| o.iter().any(|x| x == RUN_WITHOUT))
}

/// The setup card `card` of thread `thread`'s loop, shown and, on a terminal (`consent`),
/// answered: start and save (then the loop it started), change… (the editor, then the card
/// again), cancel, or, on a card that holds a task, run without a loop. Without a terminal
/// the card stays open and the `twr respond` lines that answer it are printed: exit 2.
async fn answer_card(
    c: &mut twr_core::client::Client,
    socket: &Path,
    thread: &str,
    mut card: Value,
    consent: Consent<'_>,
) -> anyhow::Result<ExitCode> {
    let Consent::Ask(ask) = consent else {
        outln!("{}", card_text(&card));
        std::io::stdout().flush()?;
        errln!(
            "twr: no terminal to answer it on: the card stays open; start the loop and save \
             these settings with `{}`{}, or answer it in the TUI or the apps",
            respond_line(&card, START),
            if offers_run_without(&card) {
                format!(
                    ", send the task as your own message with no loop with `{}`",
                    respond_line(&card, RUN_WITHOUT)
                )
            } else {
                String::new()
            }
        );
        return Ok(ExitCode::from(LEFT_OPEN));
    };
    loop {
        outln!("{}", card_text(&card));
        std::io::stdout().flush()?;
        let without = offers_run_without(&card);
        let reply = ask(if without {
            "1 start and save  2 change…  3 cancel  4 run without a loop: "
        } else {
            "1 start and save  2 change…  3 cancel: "
        })?;
        match card_reply(&reply, without) {
            CardReply::Start => {
                c.call("attention.respond", answer_params(&card, START))
                    .await?;
                let detail = c.call("loop.get", json!({ "thread": thread })).await?;
                outln!("{}", started_text(&detail));
                return Ok(ExitCode::SUCCESS);
            }
            CardReply::Cancel => {
                c.call("attention.respond", answer_params(&card, CANCEL))
                    .await?;
                outln!("cancelled: no loop started, nothing saved");
                return Ok(ExitCode::SUCCESS);
            }
            CardReply::RunWithout => {
                c.call("attention.respond", answer_params(&card, RUN_WITHOUT))
                    .await?;
                outln!(
                    "sent the task to {} as your own message: no loop, nothing saved",
                    str_of(&card["thread_name"])
                );
                return Ok(ExitCode::SUCCESS);
            }
            CardReply::Change => match change_card(c, socket, &card).await {
                Ok(Some(again)) => card = again,
                Ok(None) => outln!("no changes"),
                Err(e) => errln!("twr: {}", localize(&format!("{e:#}"))),
            },
            CardReply::Open => {
                outln!(
                    "the card stays open: start the loop and save these settings with `{}`, or \
                     answer it in the TUI or the apps",
                    respond_line(&card, START)
                );
                return Ok(ExitCode::from(LEFT_OPEN));
            }
        }
    }
}

/// `twr thread new … --loop implement`: what `thread.new`'s answer `t` says of the loop it
/// asked for (`t["loop"]`, `loop.request`'s answer, or an error the thread was made despite):
/// the loop it started, or its card, answered on a terminal (`consent`) as `twr loop implement`
/// does. `json`: the whole answer, with the card. Exit 2 while the card stays open; 1 when the
/// loop could not be asked for (the thread stays, idle).
pub(super) async fn after_new_thread(
    socket: &Path,
    t: Value,
    json: bool,
    consent: Consent<'_>,
) -> anyhow::Result<ExitCode> {
    let thread = str_of(&t["id"]).to_string();
    let name = str_of(&t["name"]).to_string();
    let l = t["loop"].clone();
    let mut c = connect(socket).await?;
    match l["status"].as_str() {
        Some("started") => {
            let detail = c.call("loop.get", json!({ "thread": thread })).await?;
            if json {
                let mut t = t;
                t["loop"]["detail"] = detail;
                outln!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                outln!("{}", started_text(&detail));
            }
            Ok(ExitCode::SUCCESS)
        }
        Some("needs_setup") => {
            let card = c
                .call("loop.setup.get", json!({ "attn_id": l["attn_id"] }))
                .await?;
            if json {
                let mut t = t;
                add_card(&mut t["loop"], card);
                outln!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(ExitCode::from(LEFT_OPEN));
            }
            answer_card(&mut c, socket, &thread, card, consent).await
        }
        _ => {
            if json {
                outln!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                errln!(
                    "twr: {name} was made, but its loop did not start: {} ({}); the thread is \
                     idle and the task was not sent",
                    localize(l["message"].as_str().unwrap_or("no answer from the hub")),
                    l["code"].as_str().unwrap_or("error")
                );
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// What the user answered at the card's prompt.
#[derive(Debug, PartialEq)]
enum CardReply {
    Start,
    Change,
    Cancel,
    /// Run the held task without a loop (only on a card that holds one).
    RunWithout,
    /// Anything else: the card stays open.
    Open,
}

/// The answer `reply` gives a card (`without`: one that offers [`RUN_WITHOUT`]).
fn card_reply(reply: &str, without: bool) -> CardReply {
    match reply.trim().to_ascii_lowercase().as_str() {
        "1" | "start" | START => CardReply::Start,
        "2" | "change" | "change…" | "change..." => CardReply::Change,
        "3" | CANCEL => CardReply::Cancel,
        "4" | "run" | "without" | RUN_WITHOUT if without => CardReply::RunWithout,
        _ => CardReply::Open,
    }
}

/// `attention.respond`'s parameters answering `card` (`loop.setup.get`'s) with `answer`, at the
/// revision shown.
fn answer_params(card: &Value, answer: &str) -> Value {
    json!({ "attn_id": card["attn_id"], "revision": card["revision"], "answer": answer })
}

/// The `twr respond` line that answers `card` with `answer` later (`START`: starts the loop).
fn respond_line(card: &Value, answer: &str) -> String {
    format!(
        "twr{} respond {} \"{answer}\"",
        workspace_hint(),
        revisioned_id(card)
    )
}

/// The card as the terminal shows it: its summary, then its id.
fn card_text(card: &Value) -> String {
    format!(
        "{}\n  card: {}",
        localize(str_of(&card["summary"])),
        revisioned_id(card)
    )
}

/// `2 change…`: the card's settings in the editor, then `loop.setup.update` at the revision
/// shown; the card raised again (`loop.setup.get`), or `None` when nothing changed.
async fn change_card(
    c: &mut twr_core::client::Client,
    socket: &Path,
    card: &Value,
) -> anyhow::Result<Option<Value>> {
    let setup = &card["setup"];
    let settings: LoopSettings = serde_json::from_value(setup["settings"].clone())
        .context("the card's settings do not read")?;
    // What the card's thread's box has (a remote box's own CLIs).
    let avail = c
        .call("loop.settings.get", json!({"thread": card["thread"]}))
        .await?;
    let thread = str_of(&card["thread_name"]).to_string();
    let implement = setup["kind"] == "implement";
    let doc = render_doc(
        &settings,
        &DocFor::Card {
            thread: &thread,
            implement,
            project: setup["project"].as_str(),
            save_for: setup["save_for"].as_str().unwrap_or("all"),
            scope: setup["scope"].as_str(),
        },
        &avail,
    );
    let Some((text, tmp)) = edit_in_editor(&doc, "twr-loop-card")? else {
        return Ok(None);
    };
    let edited = match parse_doc(&text, &settings, true).and_then(|e| match e {
        Some(e) if implement && e.scope.is_some() => bail!(
            "scope: an implementation loop has no scope (what the worktree holds when it \
                 starts is its baseline)"
        ),
        e => Ok(e),
    }) {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(None),
        Err(e) => bail!(
            "{e:#}; the card is unchanged (your edit is kept in {})",
            tmp.keep().display()
        ),
    };
    let mut params = json!({
        "attn_id": card["attn_id"],
        "revision": card["revision"],
        "settings": edited.settings,
    });
    if let Some(s) = &edited.save_for {
        params["save_for"] = json!(s);
    }
    if let Some(s) = &edited.scope {
        params["scope"] = json!(s);
    }
    // The editor may have taken a while: a connection of its own.
    let updated = async {
        let mut c = connect(socket).await?;
        c.call("loop.setup.update", params).await?;
        c.call("loop.setup.get", json!({ "attn_id": card["attn_id"] }))
            .await
    }
    .await;
    match updated {
        Ok(again) => Ok(Some(again)),
        Err(e) => Err(e.context(format!(
            "the card is unchanged (your edit is kept in {})",
            tmp.keep().display()
        ))),
    }
}

/// `pause`, `resume`, `stop`, `skip_to_verify` of thread `thread`'s loop.
async fn control(
    socket: &Path,
    thread: &str,
    action: &str,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let r = connect(socket)
        .await?
        .call(
            "loop.control",
            json!({ "thread": thread, "action": action }),
        )
        .await?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        outln!("{}", control_text(action, &r));
    }
    Ok(ExitCode::SUCCESS)
}

fn control_text(action: &str, r: &Value) -> String {
    let name = str_of(&r["thread_name"]);
    if r["running"] == false {
        let how = r["last"]["outcome"].as_str().unwrap_or("stopped");
        return format!("{name}'s loop ended: {}", outcome_words(how));
    }
    let done = match action {
        "pause" => "paused",
        "resume" => "resumed",
        "skip_to_verify" => "skips to verification after this round",
        _ => "changed",
    };
    format!("{name}'s loop {done} ({})", str_of(&r["line"]))
}

/// `twr loop report`: the report of the thread's loop as the hub reads it (`loop.report`).
async fn report(socket: &Path, thread: &str, json: bool) -> anyhow::Result<ExitCode> {
    let r = connect(socket)
        .await?
        .call("loop.report", json!({ "thread": thread }))
        .await?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(ExitCode::SUCCESS);
    }
    let Some(text) = r["text"].as_str() else {
        let name = str_of(&r["thread_name"]);
        let note = r["note"].as_str().unwrap_or("there is no report to read");
        if r["running"] == true {
            bail!(
                "{}; `twr{w} loop show {name}` shows where it is",
                localize(note),
                w = workspace_hint()
            );
        }
        bail!("{}", localize(note));
    };
    out!("{text}");
    if !text.ends_with('\n') {
        outln!();
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// What `twr loop settings set` changes.
#[derive(Debug, Default)]
struct Changes {
    preset: Option<String>,
    profile: Option<String>,
    implementer: Option<String>,
    workers: Option<String>,
    reviewer: Option<String>,
    commit: Option<String>,
    implementation_commit: Option<String>,
    plan_check: Option<String>,
    sweep: Option<String>,
}

/// The settings `twr loop settings set` and `--edit` start from: those that apply to
/// `project` now, else what the first card shows.
fn base_settings(got: &Value) -> anyhow::Result<LoopSettings> {
    let v = if got["settings"].is_object() {
        &got["settings"]
    } else {
        &got["first_card"]
    };
    serde_json::from_value(v.clone()).context("the hub's loop settings do not read")
}

async fn settings_set(
    socket: &Path,
    project: Option<String>,
    ch: Changes,
    json: bool,
) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let got = c
        .call("loop.settings.get", project_params(&project))
        .await?;
    let mut s = base_settings(&got)?;
    if let Some(p) = &ch.preset {
        let id = preset_id(p);
        let preset = got["presets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["id"] == id)
            .ok_or_else(|| anyhow!("the hub has no preset {id}"))?;
        let preset: twr_core::loops::settings::Preset =
            serde_json::from_value(preset.clone()).context("the hub's preset does not read")?;
        s = s.with_preset(&preset);
    }
    let parse = |e: String| anyhow!(e);
    if let Some(p) = ch.profile {
        s.profile = p;
    }
    if let Some(i) = ch.implementer {
        s.implementer = parse_implementer(&i).map_err(parse)?;
    }
    if let Some(w) = ch.workers {
        s.workers = WorkersChoice::parse(&w).map_err(parse)?;
    }
    if let Some(r) = ch.reviewer {
        s.reviewer = ReviewerChoice::parse(&r).map_err(parse)?;
    }
    if let Some(k) = ch.commit {
        s.review_commit = ReviewCommit::parse(&k).map_err(parse)?;
    }
    if let Some(k) = ch.implementation_commit {
        s.commit = ImplCommit::parse(&k).map_err(parse)?;
    }
    if let Some(p) = ch.plan_check {
        s.plan_check = match p.as_str() {
            "profile" => None,
            p => Some(PlanCheck::parse(p).map_err(parse)?),
        };
    }
    if let Some(w) = ch.sweep {
        s.sweep = sweep_mode(&w).map_err(parse)?;
    }
    let mut params = json!({ "settings": s });
    if let Some(p) = &project {
        params["project"] = json!(p);
    }
    let r = c.call("loop.settings.set", params).await?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        outln!("{}", saved_text(&s, project.as_deref()));
    }
    Ok(())
}

fn saved_text(s: &LoopSettings, project: Option<&str>) -> String {
    let whose = match project {
        Some(p) => format!("{p}'s own"),
        None => "the workspace's".to_string(),
    };
    format!(
        "saved as {whose} loop settings:\n{}",
        settings_rows(s, None, None)
            .iter()
            .map(|r| format!("  {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

async fn settings_reset(
    socket: &Path,
    project: Option<String>,
    consent: Consent<'_>,
    json: bool,
) -> anyhow::Result<()> {
    let what = match &project {
        Some(p) => format!("{p}'s own loop settings (it uses the workspace's again)"),
        None => "every saved loop setting (the next loop shows its settings card)".to_string(),
    };
    match consent {
        Consent::Yes => {}
        Consent::Ask(ask) => {
            let reply = ask(&format!("Forget {what}? [y/N] "))?;
            if !matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                outln!("nothing forgotten");
                return Ok(());
            }
        }
        Consent::NoTerminal => {
            bail!("not forgetting {what} without confirmation (no terminal to ask on): pass --yes")
        }
    }
    let r = connect(socket)
        .await?
        .call("loop.settings.reset", project_params(&project))
        .await?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
    } else if r["reset"] == true {
        outln!("forgot {what}");
    } else {
        outln!("nothing was saved");
    }
    Ok(())
}

async fn settings_edit(socket: &Path, project: Option<String>, json: bool) -> anyhow::Result<()> {
    let got = connect(socket)
        .await?
        .call("loop.settings.get", project_params(&project))
        .await?;
    let base = base_settings(&got)?;
    let doc = render_doc(
        &base,
        &DocFor::Saved {
            project: project.as_deref(),
            saved: got["settings"].is_object(),
        },
        &got,
    );
    let Some((text, tmp)) = edit_in_editor(&doc, "twr-loop-settings")? else {
        outln!("no changes");
        return Ok(());
    };
    let edited = match parse_doc(&text, &base, false) {
        Ok(Some(e)) => e,
        Ok(None) => {
            outln!("no changes");
            return Ok(());
        }
        Err(e) => bail!(
            "{e:#}; nothing was saved (your edit is kept in {})",
            tmp.keep().display()
        ),
    };
    let mut params = json!({ "settings": edited.settings });
    if let Some(p) = &project {
        params["project"] = json!(p);
    }
    let saved = async {
        connect(socket)
            .await?
            .call("loop.settings.set", params)
            .await
    }
    .await;
    let r = match saved {
        Ok(r) => r,
        Err(e) => {
            return Err(e.context(format!(
                "saving the loop settings failed; your edit is kept in {}",
                tmp.keep().display()
            )))
        }
    };
    if json {
        outln!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        outln!("{}", saved_text(&edited.settings, project.as_deref()));
    }
    Ok(())
}

/// Opens `text` in the user's editor (`$VISUAL`, else `$EDITOR`, else `vi`) in a private
/// temporary file named `prefix-…​.toml`: the edited text and the file (kept until the caller
/// saved what it holds, [`TempFile::keep`] when that fails), or `None` when it was left as it
/// was. As `twr context edit`: a failure after the editor ran keeps the copy and names it.
fn edit_in_editor(text: &str, prefix: &str) -> anyhow::Result<Option<(String, TempFile)>> {
    let tmp = TempFile::create(prefix, ".toml", text.as_bytes())?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("twr-loop-edit")
        .arg(tmp.path())
        .status();
    let after = match std::fs::read(tmp.path()) {
        Ok(a) => a,
        Err(e) => {
            let kept = tmp.keep();
            return Err(anyhow!(e)).with_context(|| {
                format!(
                    "reading the edited settings failed; nothing changed (the edited copy, if \
                     any, is kept in {})",
                    kept.display()
                )
            });
        }
    };
    let edited = after != text.as_bytes();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) if edited => bail!(
            "the editor `{editor}` exited with {s}; nothing changed (the edited copy is kept in \
             {})",
            tmp.keep().display()
        ),
        Ok(s) => bail!("the editor `{editor}` exited with {s}; nothing changed"),
        Err(e) if edited => {
            let kept = tmp.keep();
            return Err(anyhow!(e)).with_context(|| {
                format!(
                    "running the editor `{editor}` failed; nothing changed (the edited copy is \
                     kept in {})",
                    kept.display()
                )
            });
        }
        Err(e) => return Err(anyhow!(e)).with_context(|| format!("running the editor `{editor}`")),
    }
    if !edited {
        return Ok(None);
    }
    match String::from_utf8(after) {
        Ok(a) => Ok(Some((a, tmp))),
        Err(_) => bail!(
            "the edited settings are not UTF-8; nothing changed (the edit is kept in {})",
            tmp.keep().display()
        ),
    }
}

/// What the TOML document edits.
enum DocFor<'a> {
    /// Saved settings: the workspace's, or `project`'s own; `saved`: there are some (else the
    /// first card's are shown).
    Saved {
        project: Option<&'a str>,
        saved: bool,
    },
    /// A setup card of `thread` (project `project`), which also says where its answer saves
    /// (`save_for`) and what the run reviews (`scope`).
    Card {
        thread: &'a str,
        implement: bool,
        project: Option<&'a str>,
        save_for: &'a str,
        scope: Option<&'a str>,
    },
}

/// The TOML string `s`.
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The settings `s` as the commented TOML document the editor opens, for `what`, with what
/// the hub found installed (`got`: `loop.settings.get`'s `runtimes` and `profiles`) in its
/// comments.
fn render_doc(s: &LoopSettings, what: &DocFor<'_>, got: &Value) -> String {
    let mut out = String::new();
    let head = match what {
        DocFor::Saved {
            project: Some(p),
            saved,
        } => format!(
            "# twr loop settings: {p}'s own{}. Saving them makes them {p}'s own.",
            if *saved {
                ""
            } else {
                " (none saved yet: these are what would apply)"
            }
        ),
        DocFor::Saved {
            project: None,
            saved,
        } => format!(
            "# twr loop settings: the workspace's (every project without its own){}.",
            if *saved {
                ""
            } else {
                " (none saved yet: these are what the first settings card shows)"
            }
        ),
        DocFor::Card {
            thread, implement, ..
        } => format!(
            "# The settings card of {thread}'s {} loop. Saving this changes the card; the loop\n\
             # starts, and they are saved, only when you answer it `1 start and save`.",
            if *implement {
                "implementation"
            } else {
                "review"
            }
        ),
    };
    out.push_str(&head);
    out.push_str(
        "\n# Change the values and save. Leave the file as it is to change nothing. Lines\n\
         # starting with # are comments.\n\n",
    );
    let profiles: Vec<&str> = got["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let profiles = if profiles.is_empty() {
        "low, medium, high, or a [loops.profiles.<name>] of your config".to_string()
    } else {
        profiles.join(", ")
    };
    let lines = [
        format!("# How much review a loop runs: {profiles}."),
        format!("profile = {}", toml_str(&s.profile)),
        String::new(),
        "# The implementer: \"thread\" (the thread's own model), or runtime[:account]/model[:effort]"
            .into(),
        "# on the thread's own runtime (a loop never moves a thread to another runtime).".into(),
        format!(
            "implementer = {}",
            toml_str(
                &s.implementer
                    .as_ref()
                    .map_or_else(|| "thread".to_string(), |m| m.to_string())
            )
        ),
        String::new(),
        "# The workers: the implementer's subagents, model[:effort], where its runtime has them."
            .into(),
        format!("workers = {}", toml_str(&s.workers.to_string())),
        String::new(),
        "# The reviewer: runtime[:account]/model (claude-code, codex, grok-build, or an acp preset"
            .into(),
        "# below that names a read-only mode); its effort is each round's, from the profile."
            .into(),
        format!("reviewer = {}", toml_str(&s.reviewer.to_string())),
        String::new(),
        "# What a review loop commits: \"match\" (the fixes, when the thread commits its work) or"
            .into(),
        "# \"none\".".into(),
        format!("commit = {}", toml_str(s.review_commit.as_str())),
        String::new(),
        "# What an implementation loop commits: \"end\" (one commit of its work at its end),".into(),
        "# \"per-round\" (each step that changed something commits it) or \"none\".".into(),
        format!("implementation_commit = {}", toml_str(s.commit.as_str())),
        String::new(),
        "# When an implementation loop has its plan reviewed: \"profile\" (the profile's),".into(),
        "# \"contracts\" (when the plan touches a contract), \"always\" or \"never\".".into(),
        format!(
            "plan_check = {}",
            toml_str(s.plan_check.map_or("profile", PlanCheck::as_str))
        ),
        String::new(),
        "# Who audits the sweep's rules: \"thread\" (the thread, with its own subagents) or".into(),
        "# \"runs\" (one read-only reviewer run per rule, in parallel: one reviewer call per rule)."
            .into(),
        format!("sweep = {}", toml_str(sweep_words(s.sweep))),
    ];
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }
    if let DocFor::Card {
        implement,
        project,
        save_for,
        scope,
        ..
    } = what
    {
        out.push('\n');
        match project {
            Some(p) => out.push_str(&format!(
                "# Where `start and save` saves them: \"all\" (every project without its own) or\n\
                 # \"project\" ({p}'s own).\n"
            )),
            None => out.push_str(
                "# Where `start and save` saves them: \"all\" (this thread has no project of its \
                 own).\n",
            ),
        }
        out.push_str(&format!("save_for = {}\n", toml_str(save_for)));
        // (An implementation loop has no scope: its baseline is what the worktree holds.)
        if !implement {
            out.push_str(
                "\n# What this run reviews: \"branch\", \"uncommitted\", \"since <ref>\" or \"last <n>\".\n",
            );
            out.push_str(&format!(
                "scope = {}\n",
                toml_str(scope.unwrap_or("branch"))
            ));
        }
    }
    let runtimes: Vec<String> = got["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(runtime_words)
        .collect();
    if !runtimes.is_empty() {
        match got["box"].as_str() {
            Some(b) => out.push_str(&format!(
                "\n# On box {b} (the thread's, where its reviewer runs):\n"
            )),
            None => out.push_str("\n# On the hub's machine:\n"),
        }
        for r in runtimes {
            out.push_str(&format!("#   {r}\n"));
        }
    }
    out
}

/// One runtime of `loop.settings.get`'s `runtimes`, in words. An ACP preset says whether it
/// can review (its read-only mode) or why not.
fn runtime_words(r: &Value) -> String {
    let name = str_of(&r["runtime"]);
    let acp = r["acp"] == true;
    if acp {
        if let Some(why) = r["cannot_review"].as_str() {
            return format!("{name}: can't review ({why})");
        }
    }
    if r["found"] != true {
        return format!("{name}: not installed");
    }
    let mut parts = vec![if acp {
        format!(
            "acp preset, reviews in its {:?} mode",
            str_of(&r["review_mode"])
        )
    } else if r["login"] == true {
        "logged in".to_string()
    } else {
        "not logged in".to_string()
    }];
    let list = |k: &str| -> Vec<&str> {
        r[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    for (k, label) in [
        ("models", "models"),
        ("efforts", "efforts"),
        ("accounts", "accounts"),
    ] {
        let l = list(k);
        if !l.is_empty() {
            parts.push(format!("{label} {}", l.join(", ")));
        }
    }
    if r["subagents"] == true {
        parts.push("runs subagents".into());
    }
    format!("{name}: {}", parts.join("; "))
}

/// The editor's document, as read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    profile: String,
    implementer: String,
    workers: String,
    reviewer: String,
    commit: String,
    #[serde(default)]
    implementation_commit: Option<String>,
    #[serde(default)]
    plan_check: Option<String>,
    #[serde(default)]
    sweep: Option<String>,
    #[serde(default)]
    save_for: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// What an edited document says.
#[derive(Debug, PartialEq)]
struct Edited {
    settings: LoopSettings,
    save_for: Option<String>,
    scope: Option<String>,
}

/// Reads an edited document (`card`: a setup card's, which may say `save_for` and `scope`)
/// into settings, the rest of `base` kept: `None` when it is empty (comments only), an error
/// naming the key that does not read.
fn parse_doc(text: &str, base: &LoopSettings, card: bool) -> anyhow::Result<Option<Edited>> {
    let empty = text
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
    if empty {
        return Ok(None);
    }
    let doc: Doc = toml::from_str(text).map_err(|e| anyhow!("the settings do not read: {e}"))?;
    let key = |k: &'static str| move |e: String| anyhow!("{k}: {e}");
    let mut s = base.clone();
    check_profile_name(doc.profile.trim()).map_err(key("profile"))?;
    s.profile = doc.profile.trim().to_string();
    s.implementer = parse_implementer(&doc.implementer).map_err(key("implementer"))?;
    s.workers = WorkersChoice::parse(&doc.workers).map_err(key("workers"))?;
    s.reviewer = ReviewerChoice::parse(&doc.reviewer).map_err(key("reviewer"))?;
    s.review_commit = ReviewCommit::parse(&doc.commit).map_err(key("commit"))?;
    if let Some(c) = &doc.implementation_commit {
        s.commit = ImplCommit::parse(c).map_err(key("implementation_commit"))?;
    }
    if let Some(p) = &doc.plan_check {
        s.plan_check = match p.trim() {
            "profile" => None,
            "never" => Some(PlanCheck::Never),
            "contracts" => Some(PlanCheck::Contracts),
            "always" => Some(PlanCheck::Always),
            other => bail!(
                "plan_check: {other:?} is not a plan check (profile, never, contracts or always)"
            ),
        };
    }
    if let Some(w) = &doc.sweep {
        s.sweep = sweep_mode(w.trim()).map_err(key("sweep"))?;
    }
    if !card && (doc.save_for.is_some() || doc.scope.is_some()) {
        bail!("save_for and scope belong to a loop's settings card, not to saved settings");
    }
    let save_for = match doc.save_for.as_deref().map(str::trim) {
        None => None,
        Some(w @ ("all" | "project")) => Some(w.to_string()),
        Some(other) => bail!("save_for: {other:?} is neither \"all\" nor \"project\""),
    };
    let scope = doc
        .scope
        .as_deref()
        .map(twr_core::slash::scope_value)
        .transpose()
        .map_err(key("scope"))?;
    Ok(Some(Edited {
        settings: s,
        save_for,
        scope,
    }))
}

// ---------------------------------------------------------------------------------------------
// Text for people
// ---------------------------------------------------------------------------------------------

/// How a loop ended (`loop.ended` `outcome`), in words.
fn outcome_words(outcome: &str) -> &str {
    match outcome {
        "clean" => "clean",
        "ledger_fixed_point" => "at a ledger fixed point (every finding already ledgered)",
        "cap_expired" => "its verification cap spent with findings still arriving",
        "stopped" => "stopped",
        "failed" => "failed",
        other => other,
    }
}

/// Why the ladder ended (`loop.transition` `reason`), in words.
fn transition_words(reason: &str) -> &str {
    match reason {
        "converged" => "the last tier converged",
        "no_new_class" => "two rounds in a row found nothing of a new class",
        "backstop" => "a tier reached the backstop",
        "max_rounds" => "the last tier ran its profile's rounds",
        "skipped" => "skipped to verification",
        other => other,
    }
}

/// `thread` or `runs`, as the settings write a sweep mode.
fn sweep_words(m: twr_core::loops::config::SweepMode) -> &'static str {
    match m {
        twr_core::loops::config::SweepMode::Thread => "thread",
        twr_core::loops::config::SweepMode::Runs => "runs",
    }
}

/// The sweep mode `w` names.
fn sweep_mode(w: &str) -> Result<twr_core::loops::config::SweepMode, String> {
    match w {
        "thread" => Ok(twr_core::loops::config::SweepMode::Thread),
        "runs" => Ok(twr_core::loops::config::SweepMode::Runs),
        other => Err(format!("{other:?} is neither \"thread\" nor \"runs\"")),
    }
}

/// The rows of settings `s` (`runtime · model · effort`) for a loop of `kind` (`review`,
/// `implement`; `None`: saved settings, which both kinds share), with this run's `overrides`.
fn settings_rows(s: &LoopSettings, overrides: Option<&Value>, kind: Option<&str>) -> Vec<String> {
    let mut rows = vec![
        format!("profile      {}", s.profile),
        format!(
            "implementer  {}",
            s.implementer
                .as_ref()
                .map_or_else(|| "the thread's own".to_string(), |m| m.to_string())
        ),
        format!("workers      {}", s.workers),
        format!("reviewer     {}", s.reviewer),
    ];
    if s.sweep == twr_core::loops::config::SweepMode::Runs {
        rows.push("sweep        runs (one reviewer call per rule)".into());
    }
    let plan_check = s.plan_check.map_or("the profile's", PlanCheck::as_str);
    match kind {
        Some("review") => rows.push(format!("commit       {}", s.review_commit.as_str())),
        Some(_) => {
            rows.push(format!("plan check   {plan_check}"));
            rows.push(format!("commit       {}", s.commit.as_str()));
        }
        None => {
            rows.push(format!(
                "commit       {} (review loops), {} (implementation loops)",
                s.review_commit.as_str(),
                s.commit.as_str()
            ));
            rows.push(format!("plan check   {plan_check}"));
        }
    }
    if let Some(o) = overrides
        .and_then(Value::as_object)
        .filter(|o| !o.is_empty())
    {
        let run: Vec<String> = o
            .iter()
            .map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or_default()))
            .collect();
        rows.push(format!("this run     {}", run.join(", ")));
    }
    rows
}

/// Where a loop's settings came from (`loop.get`'s `started`).
fn source_words(started: &Value) -> String {
    match (
        started["settings_from"].as_str(),
        started["settings_of"].as_str(),
    ) {
        (Some("setup_item"), _) => "the settings card you answered".into(),
        (_, Some("project")) => "its project's own saved settings".into(),
        _ => "the workspace's saved settings".into(),
    }
}

/// A loop's kind in words (`review loop`, `implementation loop`), from `loop.get`'s `kind`.
fn kind_words(kind: &Value) -> &'static str {
    match kind.as_str() {
        Some("implement") => "implementation loop",
        _ => "review loop",
    }
}

/// The command that starts a loop of `kind` (`review`, `implement`).
fn kind_command(kind: &Value) -> &'static str {
    match kind.as_str() {
        Some("implement") => "implement",
        _ => "review",
    }
}

/// An implementation loop's task as a line: its first line, cut at 100 characters, then how
/// many more lines it has.
fn task_line(task: &str) -> String {
    let lines: Vec<&str> = task.lines().filter(|l| !l.trim().is_empty()).collect();
    let first = lines.first().map_or("", |l| l.trim());
    let mut out: String = first.chars().take(100).collect();
    if first.chars().count() > 100 {
        out.push('…');
    }
    if lines.len() > 1 {
        out.push_str(&format!(
            " (+{} more line{})",
            lines.len() - 1,
            if lines.len() == 2 { "" } else { "s" }
        ));
    }
    out
}

/// The rows of a loop's start (`loop.get`'s `started`): its settings, where they came from is
/// said by the caller; then its scope or task and baseline.
fn start_rows(started: &Value, kind: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(s) = serde_json::from_value::<LoopSettings>(started["settings"].clone()) {
        out.extend(
            settings_rows(&s, Some(&started["overrides"]), Some(kind_command(kind)))
                .into_iter()
                .map(|r| format!("  {r}")),
        );
    }
    if let Some(scope) = started["scope"].as_str() {
        out.push(format!("  scope        {scope}"));
    }
    if kind.as_str() == Some("implement") {
        out.push(format!(
            "  task         {}",
            match started["task"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                Some(t) => task_line(t),
                None => "what the conversation already settled".into(),
            }
        ));
        for b in started["baseline"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .filter(|b| b["changed"].as_u64().unwrap_or(0) > 0)
        {
            out.push(format!(
                "  baseline     {} (out of scope, left out of the commit)",
                str_of(&b["summary"])
            ));
        }
    }
    out
}

/// What `twr loop review` and `twr loop implement` print of a loop they started (`loop.get`'s
/// answer).
fn started_text(d: &Value) -> String {
    let name = str_of(&d["thread_name"]);
    let started = &d["started"];
    let kind = if d["kind"].is_null() {
        &started["kind"]
    } else {
        &d["kind"]
    };
    let mut out = vec![format!(
        "{name}: {} started ({})",
        kind_words(kind),
        d["line"].as_str().unwrap_or("-")
    )];
    out.push(format!(
        "  on {} (`twr{w} loop settings` shows them; `twr{w} loop {} {name} --setup` changes \
         them first)",
        source_words(started),
        kind_command(kind),
        w = workspace_hint()
    ));
    out.extend(start_rows(started, kind));
    out.join("\n")
}

/// `twr loop list`'s table of `loop.list`'s answer.
fn list_text(list: &Value) -> String {
    let rows: Vec<[String; 7]> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let mut waiting = str_of(&l["waiting_on"]).to_string();
            if !l["stuck"].is_null() {
                waiting.push_str(" (stuck)");
            } else if l["paused"] == true {
                waiting.push_str(" (paused)");
            }
            [
                str_of(&l["thread_name"]).to_string(),
                str_of(&l["kind"]).to_string(),
                str_of(&l["profile"]).to_string(),
                str_of(&l["phase"]).to_string(),
                l["round"].as_str().unwrap_or("-").to_string(),
                l["ledger"].as_u64().unwrap_or(0).to_string(),
                waiting,
            ]
        })
        .collect();
    if rows.is_empty() {
        return "no loop is running\n".into();
    }
    table(
        &[
            "THREAD",
            "KIND",
            "PROFILE",
            "PHASE",
            "ROUND",
            "LEDGER",
            "WAITING ON",
        ],
        &rows,
    )
}

/// The first line of `s`.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// The time of `at` (RFC 3339) as `HH:MM:SS`, in local time.
fn clock(at: &Value) -> String {
    at.as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

/// `twr loop show`'s text of `loop.get`'s answer.
fn show_text(d: &Value) -> String {
    let name = str_of(&d["thread_name"]);
    let kind = d["kind"].as_str().unwrap_or("review");
    let mut out = Vec::new();
    if d["running"] == true {
        out.push(format!(
            "{name}: {} LOOP {} (running, {})",
            kind.to_uppercase(),
            str_of(&d["line"]),
            str_of(&d["id"])
        ));
        let state = &d["state"];
        if let Some(why) = state["stuck"].as_str() {
            out.push(format!(
                "  stuck: {why} (`twr{w} loop resume {name}` asks for that step again)",
                w = workspace_hint()
            ));
        }
        out.push(format!(
            "  waiting on {}{}",
            str_of(&state["waiting_on"]),
            match state["reviewer"]["model"].as_str() {
                Some(m) if state["waiting_on"] == "review" => format!(
                    " (reviewer {m} {})",
                    state["reviewer"]["effort"].as_str().unwrap_or("")
                ),
                _ => String::new(),
            }
        ));
    } else {
        out.push(format!(
            "{name}: {} {} (ended: {})",
            kind_words(&d["kind"]),
            str_of(&d["id"]),
            outcome_words(d["outcome"].as_str().unwrap_or("-"))
        ));
    }
    let started = &d["started"];
    if started.is_object() {
        out.push(format!(
            "settings ({}, started {}):",
            source_words(started),
            clock(&started["at"])
        ));
        out.extend(start_rows(started, &d["kind"]));
        for b in started["bases"].as_array().into_iter().flatten() {
            out.push(format!(
                "  repository   {} (from {})",
                str_of(&b["repo"]),
                str_of(&b["base"])
            ));
        }
    }
    let phases = d["phases"].as_array().cloned().unwrap_or_default();
    if !phases.is_empty() {
        out.push("phases:".into());
        for p in &phases {
            out.push(format!("  {}  {}", clock(&p["at"]), str_of(&p["phase"])));
        }
    }
    if let Some(goal) = phases.iter().find_map(|p| p["goal"].as_str()) {
        out.push(format!("goal: {}", first_line(goal)));
    }
    out.extend(plan_rows(d));
    let rounds = d["rounds"].as_array().cloned().unwrap_or_default();
    if !rounds.is_empty() {
        out.push("rounds:".into());
    }
    for r in &rounds {
        let id = str_of(&r["id"]);
        let effort = r["effort"].as_str().unwrap_or("");
        // (The plan check's round reviews the plan, not the work.)
        let effort = if id == "p1" {
            format!("{effort} (plan check)")
        } else {
            effort.to_string()
        };
        if r["failed"] == true {
            out.push(format!(
                "  {id} {effort}: failed: {}",
                first_line(r["reason"].as_str().unwrap_or("-"))
            ));
            continue;
        }
        let findings = r["findings"].as_array().cloned().unwrap_or_default();
        let count = match findings.len() {
            0 if r["failed"].is_null() => "running".to_string(),
            0 => "clean".to_string(),
            1 => "1 finding".to_string(),
            n => format!("{n} findings"),
        };
        out.push(format!("  {id} {effort}: {count}"));
        let triage = &r["triage"];
        for f in &findings {
            let fid = str_of(&f["id"]);
            let verdict = triage["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["id"] == f["id"])
                .map(verdict_words)
                .unwrap_or_default();
            out.push(format!(
                "    {fid} {} {}: {}{verdict}",
                str_of(&f["severity"]),
                str_of(&f["location"]),
                first_line(str_of(&f["problem"]))
            ));
        }
        if let Some(g) = triage["gates"].as_object() {
            let ran: Vec<&str> = g
                .get("ran")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            out.push(format!(
                "    gates: {} ({})",
                g.get("result").and_then(Value::as_str).unwrap_or("-"),
                ran.join(", ")
            ));
        }
    }
    out.extend(sweep_rows(d));
    let ledger = d["ledger"].as_array().cloned().unwrap_or_default();
    out.push(format!("ledger: {}", ledger.len()));
    for e in &ledger {
        let loc = match e["location"].as_str() {
            Some(l) if !l.is_empty() => format!(" {l}"),
            _ => String::new(),
        };
        out.push(format!(
            "  {} ({}, {}){loc}: {} — {}",
            str_of(&e["id"]),
            str_of(&e["kind"]).replace('_', " "),
            str_of(&e["from"]),
            first_line(str_of(&e["problem"])),
            first_line(str_of(&e["reason"]))
        ));
    }
    if let Some(reason) = d["transition"]["reason"].as_str() {
        out.push(format!(
            "transition: after {}: {}{}",
            d["transition"]["round"].as_str().unwrap_or("-"),
            transition_words(reason),
            if d["transition"]["verify_skipped"] == true {
                "; verification skipped (it could only repeat that round)"
            } else {
                ""
            }
        ));
    }
    for c in d["controls"].as_array().into_iter().flatten() {
        out.push(format!(
            "control: {} {} by {}{}",
            clock(&c["at"]),
            str_of(&c["action"]),
            str_of(&c["by"]),
            c["why"]
                .as_str()
                .map(|w| format!(": {}", first_line(w)))
                .unwrap_or_default()
        ));
    }
    out.extend(commit_rows(d));
    if d["ended"].is_object() {
        let e = &d["ended"];
        out.push(format!(
            "ended: {} at {}{}",
            outcome_words(e["outcome"].as_str().unwrap_or("-")),
            clock(&e["at"]),
            e["reason"]
                .as_str()
                .map(|r| format!(" ({})", first_line(r)))
                .unwrap_or_default()
        ));
    }
    if let Some(f) = d["folder"].as_str() {
        out.push(format!("folder: {f}"));
    }
    if let Some(r) = d["report_path"].as_str() {
        out.push(format!("report: {r}"));
    }
    out.join("\n")
}

/// The sweep's rows of `loop.get`'s answer `d` when the hub audited it (`sweep = "runs"`):
/// each rule, its audit (findings, failed, not run) and the thread's verdict, with each
/// finding of the audit; nothing for a sweep the thread audited itself.
fn sweep_rows(d: &Value) -> Vec<String> {
    let Some(rules) = d["sweep"]["rules"].as_array() else {
        return Vec::new();
    };
    let mut out = vec![if rules.is_empty() {
        "sweep (audited by runs): no rule named".to_string()
    } else {
        "sweep (audited by runs):".to_string()
    }];
    for r in rules {
        let a = &r["audit"];
        let failures = a["failures"].as_array().map_or(0, Vec::len);
        let audit = match str_of(&a["status"]) {
            "answered" => match a["count"].as_u64().unwrap_or(0) {
                0 => "clean".to_string(),
                1 => "1 finding".to_string(),
                n => format!("{n} findings"),
            },
            "not_run" => format!(
                "not run: {}",
                first_line(
                    a["failures"]
                        .as_array()
                        .and_then(|f| f.last())
                        .and_then(Value::as_str)
                        .unwrap_or("-")
                )
            ),
            "failed" => "failed, retrying".to_string(),
            "running" if failures > 0 => "running (retry)".to_string(),
            "running" => "running".to_string(),
            _ => "waiting".to_string(),
        };
        let verdict = match r["verdict"]["result"].as_str() {
            Some(v) => format!(
                " → {}{}",
                v.replace('_', " "),
                r["verdict"]["reason"]
                    .as_str()
                    .map(|w| format!(" ({})", first_line(w)))
                    .unwrap_or_default()
            ),
            None => String::new(),
        };
        out.push(format!(
            "  {} {}: {audit}{verdict}",
            str_of(&a["id"]),
            str_of(&r["name"])
        ));
        out.push(format!(
            "    rule: {} (where: {})",
            first_line(str_of(&r["rule"])),
            first_line(str_of(&r["where"]))
        ));
        for f in a["findings"].as_array().into_iter().flatten() {
            out.push(format!(
                "    {} {} {}: {}",
                str_of(&f["id"]),
                str_of(&f["severity"]),
                str_of(&f["location"]),
                first_line(str_of(&f["problem"]))
            ));
        }
    }
    out
}

/// An implementation loop's plan and implement rows of `loop.get`'s answer `d`: each plan
/// (revision, contract risk, path) and each implement turn's result.
fn plan_rows(d: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for p in d["plans"].as_array().into_iter().flatten() {
        let risk = match p["contract_risk"].as_bool() {
            Some(true) => "contract risk: yes",
            Some(false) => "contract risk: no",
            None => "contract risk: -",
        };
        out.push(format!(
            "plan {}: {risk}{} ({})",
            p["revision"].as_u64().unwrap_or(1),
            p["reason"]
                .as_str()
                .filter(|r| !r.is_empty())
                .map(|r| format!(" — {}", first_line(r)))
                .unwrap_or_default(),
            p["path"].as_str().unwrap_or("-")
        ));
    }
    for i in d["implemented"].as_array().into_iter().flatten() {
        let ran: Vec<&str> = i["gates"]["ran"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        out.push(format!(
            "implement: {}, gates {}{}{}",
            str_of(&i["status"]),
            i["gates"]["result"].as_str().unwrap_or("-"),
            if ran.is_empty() {
                String::new()
            } else {
                format!(" ({})", ran.join(", "))
            },
            if i["accepted"] == false {
                " — sent back: gates must be green before review"
            } else {
                ""
            }
        ));
    }
    out
}

/// The commit rows of `loop.get`'s answer `d`: each check of the hub's (passed or what was
/// wrong, and its notes), then, once the loop ended, what it committed and what it did not.
/// A repository is committed or has a problem, never both.
fn commit_rows(d: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for c in d["commit_checks"].as_array().into_iter().flatten() {
        let problems: Vec<&str> = c["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        out.push(if problems.is_empty() {
            "commit check: passed".to_string()
        } else {
            format!("commit check: failed: {}", problems.join("; "))
        });
        for n in c["notes"].as_array().into_iter().flatten() {
            out.push(format!("  note: {}", str_of(n)));
        }
    }
    let e = &d["ended"];
    for c in e["commits"].as_array().into_iter().flatten() {
        out.push(format!(
            "committed: {} ({})",
            str_of(&c["sha"]),
            str_of(&c["repo"])
        ));
    }
    if let Some(p) = e["commit_problem"].as_str() {
        out.push(format!("not committed: {p}"));
    }
    out
}

/// ` — fixed (new-class, bounds)`, ` — dismissed: <reason>`, ` — duplicate of L1`.
fn verdict_words(item: &Value) -> String {
    match item["verdict"].as_str() {
        Some("fixed") => {
            let mut what: Vec<&str> = Vec::new();
            what.extend(item["class"].as_str());
            what.extend(item["rule"].as_str());
            if what.is_empty() {
                " — fixed".into()
            } else {
                format!(" — fixed ({})", what.join(", "))
            }
        }
        Some("dismissed") => format!(
            " — dismissed: {}",
            first_line(item["reason"].as_str().unwrap_or("-"))
        ),
        Some("duplicate") => format!(" — duplicate of {}", str_of(&item["of"])),
        Some(other) => format!(" — {other}"),
        None => String::new(),
    }
}

/// `twr loop settings`'s text of `loop.settings.get`'s answer for `project`.
fn settings_text(got: &Value, project: Option<&str>) -> String {
    let mut out = Vec::new();
    let saved = got["settings"].is_object();
    let shown = if saved {
        &got["settings"]
    } else {
        &got["first_card"]
    };
    out.push(match (got["source"].as_str(), project) {
        (Some("project"), Some(p)) => format!("{p}'s own loop settings ({}):", str_of(&got["id"])),
        (Some("workspace"), Some(p)) => format!(
            "the workspace's loop settings, which {p} uses ({}; `twr loop settings set --project \
             {p}` gives it its own):",
            str_of(&got["id"])
        ),
        (Some(_), _) => format!("the workspace's loop settings ({}):", str_of(&got["id"])),
        (None, _) => "no loop settings saved yet: the first loop shows its settings card, with \
                      these (from [loops] of the config, else twr's defaults):"
            .into(),
    });
    if let Ok(s) = serde_json::from_value::<LoopSettings>(shown.clone()) {
        out.extend(
            settings_rows(&s, None, None)
                .into_iter()
                .map(|r| format!("  {r}")),
        );
    }
    let projects: Vec<&str> = got["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !projects.is_empty() {
        out.push(format!(
            "projects with settings of their own: {}",
            projects.join(", ")
        ));
    }
    for w in got["warnings"].as_array().into_iter().flatten() {
        out.push(format!("warning: {}", str_of(w)));
    }
    for p in got["problems"].as_array().into_iter().flatten() {
        out.push(format!(
            "problem: {} (the next loop shows its settings card)",
            str_of(p)
        ));
    }
    let runtimes: Vec<String> = got["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(runtime_words)
        .collect();
    if !runtimes.is_empty() {
        out.push("on the hub's machine:".into());
        out.extend(runtimes.into_iter().map(|r| format!("  {r}")));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<super::super::Cli, clap::Error> {
        super::super::Cli::try_parse_from(std::iter::once("twr").chain(args.iter().copied()))
    }

    fn loop_cmd(args: &[&str]) -> LoopCmd {
        match parse(args).unwrap().cmd {
            Some(super::super::Cmd::Loop { cmd }) => cmd,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    /// `twr loop review` checks every value with the loop settings' parsers, writes `--scope`
    /// as the hub reads it, and has no flag that starts a loop without its card.
    #[test]
    fn review_parses() {
        match loop_cmd(&[
            "loop",
            "review",
            "api",
            "--setup",
            "--scope",
            "since:main",
            "--profile",
            "low",
            "--reviewer",
            "claude-code/claude-opus-5-5",
            "--implementer",
            "thread",
            "--workers",
            "claude-opus-5-5:high",
            "--commit",
            "none",
            "--json",
        ]) {
            LoopCmd::Review {
                thread,
                setup,
                scope,
                profile,
                reviewer,
                implementer,
                workers,
                commit,
                json,
            } => {
                assert_eq!(thread, "api");
                assert!(setup && json);
                assert_eq!(scope.as_deref(), Some("since main"));
                assert_eq!(profile.as_deref(), Some("low"));
                assert_eq!(reviewer.as_deref(), Some("claude-code/claude-opus-5-5"));
                assert_eq!(implementer.as_deref(), Some("thread"));
                assert_eq!(workers.as_deref(), Some("claude-opus-5-5:high"));
                assert_eq!(commit.as_deref(), Some("none"));
            }
            other => panic!("{other:?}"),
        }
        for scope in ["since main", "since=main", "since:main"] {
            match loop_cmd(&["loop", "review", "api", "--scope", scope]) {
                LoopCmd::Review { scope, .. } => assert_eq!(scope.as_deref(), Some("since main")),
                other => panic!("{other:?}"),
            }
        }
        match loop_cmd(&["loop", "review", "api", "--scope=last:3"]) {
            LoopCmd::Review { scope, .. } => assert_eq!(scope.as_deref(), Some("last 3")),
            other => panic!("{other:?}"),
        }
        for bad in [
            &["loop", "review", "api", "--scope", "everything"][..],
            &["loop", "review", "api", "--reviewer", "codex"],
            &["loop", "review", "api", "--commit", "end"],
            &["loop", "review", "api", "--profile", "High"],
            &["loop", "review", "api", "--workers", "codex/x"],
            &["loop", "review", "api", "--yes"],
            &["loop", "review", "api", "--accept-defaults"],
            &["loop", "review"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let e = parse(&["loop", "review", "api", "--reviewer", "codex"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("is not runtime[:account]/model"), "{e}");
    }

    /// `twr loop implement` takes `/implement`'s flags and the task as words (`--` before words
    /// that start with `-`), or from a file; `twr thread new … --loop implement` the same, its
    /// loop flags only with `--loop`.
    #[test]
    fn implement_parses() {
        match loop_cmd(&[
            "loop",
            "implement",
            "api",
            "--setup",
            "--plan-check",
            "always",
            "--commit",
            "per-round",
            "--profile",
            "low",
            "add",
            "a",
            "CSV export",
            "--json",
        ]) {
            LoopCmd::Implement {
                thread,
                task,
                task_file,
                setup,
                commit,
                plan_check,
                profile,
                json,
                ..
            } => {
                assert_eq!(thread, "api");
                assert_eq!(task, ["add", "a", "CSV export"]);
                assert!(task_file.is_none() && setup && json);
                assert_eq!(commit.as_deref(), Some("per-round"));
                assert_eq!(plan_check.as_deref(), Some("always"));
                assert_eq!(profile.as_deref(), Some("low"));
                assert_eq!(
                    read_task(&task, None).unwrap(),
                    (Some("add a CSV export".into()), false)
                );
            }
            other => panic!("{other:?}"),
        }
        match loop_cmd(&["loop", "implement", "api", "--", "--flags", "stay"]) {
            LoopCmd::Implement { task, .. } => assert_eq!(task, ["--flags", "stay"]),
            other => panic!("{other:?}"),
        }
        match loop_cmd(&["loop", "implement", "api"]) {
            LoopCmd::Implement { task, .. } => {
                assert_eq!(read_task(&task, None).unwrap(), (None, false))
            }
            other => panic!("{other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("task.md");
        std::fs::write(&file, "\n  a long task\nover lines \n").unwrap();
        assert_eq!(
            read_task(&[], Some(&file)).unwrap(),
            (Some("a long task\nover lines".into()), false)
        );
        for bad in [
            &["loop", "implement", "api", "--commit", "match"][..],
            &["loop", "implement", "api", "--plan-check", "sometimes"],
            &["loop", "implement", "api", "--scope", "branch"],
            &[
                "loop",
                "implement",
                "api",
                "--task-file",
                "/t",
                "and",
                "words",
            ],
            &["loop", "implement"],
            &["thread", "new", ".", "--setup"],
            &["thread", "new", ".", "a", "task"],
            &["thread", "new", ".", "--loop", "review", "x"],
            &["thread", "new", ".", "--plan-check", "always"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let c = parse(&[
            "thread",
            "new",
            "api",
            "--runtime",
            "fake",
            "--loop",
            "implement",
            "--setup",
            "--commit",
            "none",
            "add",
            "rate limiting",
        ])
        .unwrap();
        let Some(super::super::Cmd::Thread {
            cmd: super::super::ThreadCmd::New {
                project, new_loop, ..
            },
        }) = c.cmd
        else {
            panic!("{:?}", c.cmd)
        };
        assert_eq!(project, "api");
        let (l, stdin) = new_thread_loop_param(new_loop).unwrap();
        assert!(!stdin);
        assert_eq!(
            l,
            json!({"kind": "implement", "task": "add rate limiting", "setup": true,
                   "overrides": {"commit": "none"}})
        );
        let c = parse(&["thread", "new", "api", "--loop", "implement"]).unwrap();
        let Some(super::super::Cmd::Thread {
            cmd: super::super::ThreadCmd::New { new_loop, .. },
        }) = c.cmd
        else {
            panic!()
        };
        let e = new_thread_loop_param(new_loop).unwrap_err().to_string();
        assert!(e.contains("needs the task"), "{e}");
    }

    #[test]
    fn the_other_commands_parse() {
        assert!(matches!(
            loop_cmd(&["loop", "list"]),
            LoopCmd::List { json: false }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "ls", "--json"]),
            LoopCmd::List { json: true }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "show", "api", "--json"]),
            LoopCmd::Show { json: true, .. }
        ));
        for (word, json) in [("pause", false), ("resume", true), ("skip", false)] {
            let mut args = vec!["loop", word, "api"];
            if json {
                args.push("--json");
            }
            let c = loop_cmd(&args);
            let ok = match c {
                LoopCmd::Pause {
                    ref thread,
                    json: j,
                } => word == "pause" && j == json && thread == "api",
                LoopCmd::Resume {
                    ref thread,
                    json: j,
                } => word == "resume" && j == json && thread == "api",
                LoopCmd::Skip {
                    ref thread,
                    json: j,
                } => word == "skip" && j == json && thread == "api",
                _ => false,
            };
            assert!(ok, "{word}: {c:?}");
        }
        assert!(matches!(
            loop_cmd(&["loop", "stop", "api", "--yes"]),
            LoopCmd::Stop { yes: true, .. }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "report", "api"]),
            LoopCmd::Report { json: false, .. }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "settings"]),
            LoopCmd::Settings {
                cmd: None,
                edit: false,
                reset: false,
                ..
            }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "settings", "--project", "api", "--edit"]),
            LoopCmd::Settings {
                cmd: None,
                edit: true,
                project: Some(_),
                ..
            }
        ));
        assert!(matches!(
            loop_cmd(&["loop", "settings", "--reset", "--yes"]),
            LoopCmd::Settings {
                reset: true,
                yes: true,
                ..
            }
        ));
        assert!(
            parse(&["loop", "settings", "--yes"]).is_err(),
            "--yes goes with --reset"
        );
        assert!(parse(&["loop", "settings", "--edit", "--reset"]).is_err());
        match loop_cmd(&[
            "loop",
            "settings",
            "set",
            "--project",
            "api",
            "--preset",
            "claude",
            "--profile",
            "medium",
            "--commit",
            "match",
        ]) {
            LoopCmd::Settings {
                cmd:
                    Some(LoopSettingsCmd::Set {
                        project,
                        preset,
                        profile,
                        commit,
                        ..
                    }),
                ..
            } => {
                assert_eq!(project.as_deref(), Some("api"));
                assert_eq!(preset.as_deref(), Some("claude"));
                assert_eq!(profile.as_deref(), Some("medium"));
                assert_eq!(commit.as_deref(), Some("match"));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["loop", "settings", "set", "--preset", "gemini"]).is_err());
        assert_eq!(preset_id("claude-codex"), "claude-code+codex");
        assert_eq!(preset_id("grok"), "grok-build");
    }

    /// The editor's document reads back to the settings it shows; an empty one changes
    /// nothing; a bad value names its key; a card's keys belong to a card.
    #[test]
    fn the_settings_document_round_trips() {
        let base = LoopSettings::first_card(&Default::default());
        let got = json!({
            "profiles": ["high", "low", "medium"],
            "runtimes": [
                {"runtime": "codex", "found": true, "login": true, "models": ["gpt-6.1-sol"],
                 "efforts": ["medium", "xhigh"], "accounts": [], "subagents": false},
                {"runtime": "grok-build", "found": false},
                {"runtime": "ex", "found": true, "login": true, "models": ["ex-large"],
                 "efforts": [], "acp": true, "review_mode": "read-only", "cannot_review": null},
                {"runtime": "bare", "found": true, "login": true, "models": [], "efforts": [],
                 "acp": true, "review_mode": null,
                 "cannot_review": "acp preset bare names no read-only mode"},
            ],
        });
        let doc = render_doc(
            &base,
            &DocFor::Saved {
                project: None,
                saved: false,
            },
            &got,
        );
        assert!(doc.contains("profile = \"high\""), "{doc}");
        assert!(doc.contains("implementer = \"thread\""), "{doc}");
        assert!(doc.contains("#   codex: logged in; models gpt-6.1-sol; efforts medium, xhigh"));
        assert!(doc.contains("#   grok-build: not installed"));
        assert!(
            doc.contains("#   ex: acp preset, reviews in its \"read-only\" mode; models ex-large")
        );
        assert!(doc.contains("#   bare: can't review (acp preset bare names no read-only mode)"));
        assert!(doc.contains("# On the hub's machine:"), "{doc}");
        // A card's thread on a remote box: what was found there, named.
        let mut on_box = got.clone();
        on_box["box"] = json!("gpu");
        let boxed = render_doc(
            &base,
            &DocFor::Saved {
                project: None,
                saved: false,
            },
            &on_box,
        );
        assert!(
            boxed.contains("# On box gpu (the thread's, where its reviewer runs):"),
            "{boxed}"
        );
        let back = parse_doc(&doc, &base, false).unwrap().unwrap();
        assert_eq!(back.settings, base);
        assert_eq!((back.save_for, back.scope), (None, None));
        let edited = doc
            .replace("profile = \"high\"", "profile = \"low\"")
            .replace(
                "reviewer = \"codex/gpt-6.1-sol\"",
                "reviewer = \"claude-code/claude-opus-5-5\"",
            )
            .replace("plan_check = \"profile\"", "plan_check = \"always\"");
        let e = parse_doc(&edited, &base, false).unwrap().unwrap();
        assert_eq!(e.settings.profile, "low");
        assert_eq!(e.settings.reviewer.runtime, "claude-code");
        assert_eq!(e.settings.plan_check, Some(PlanCheck::Always));
        assert_eq!(parse_doc("# nothing\n\n", &base, false).unwrap(), None);
        let bad = doc.replace("commit = \"match\"", "commit = \"end\"");
        let err = parse_doc(&bad, &base, false).unwrap_err().to_string();
        assert!(err.starts_with("commit: "), "{err}");
        let runs = doc.replace("sweep = \"thread\"", "sweep = \"runs\"");
        assert_eq!(
            parse_doc(&runs, &base, false)
                .unwrap()
                .unwrap()
                .settings
                .sweep,
            twr_core::loops::config::SweepMode::Runs
        );
        let bad_sweep = doc.replace("sweep = \"thread\"", "sweep = \"hub\"");
        assert!(parse_doc(&bad_sweep, &base, false)
            .unwrap_err()
            .to_string()
            .starts_with("sweep: "));
        let unknown = format!("{doc}\nmax_audit_runs = 2\n");
        assert!(parse_doc(&unknown, &base, false)
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
        // A card's document: where it saves and what it reviews.
        let card = render_doc(
            &base,
            &DocFor::Card {
                thread: "api",
                implement: false,
                project: Some("api"),
                save_for: "all",
                scope: Some("since main"),
            },
            &got,
        );
        assert!(card.contains("save_for = \"all\""), "{card}");
        assert!(card.contains("scope = \"since main\""), "{card}");
        let e = parse_doc(
            &card
                .replace("save_for = \"all\"", "save_for = \"project\"")
                .replace("scope = \"since main\"", "scope = \"last:2\""),
            &base,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(e.save_for.as_deref(), Some("project"));
        assert_eq!(e.scope.as_deref(), Some("last 2"));
        assert!(parse_doc(&card, &base, false).is_err());
        assert!(parse_doc(&card.replace("\"all\"", "\"some\""), &base, true).is_err());
        // An implementation loop's card: no scope, and its plan check and commit rows.
        let card = render_doc(
            &base,
            &DocFor::Card {
                thread: "api",
                implement: true,
                project: None,
                save_for: "all",
                scope: None,
            },
            &got,
        );
        assert!(card.contains("implementation loop"), "{card}");
        assert!(!card.contains("scope ="), "{card}");
        assert!(!card.contains("later version"), "{card}");
        let e = parse_doc(
            &card
                .replace("plan_check = \"profile\"", "plan_check = \"always\"")
                .replace(
                    "implementation_commit = \"end\"",
                    "implementation_commit = \"none\"",
                ),
            &base,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(e.settings.plan_check, Some(PlanCheck::Always));
        assert_eq!(e.settings.commit, ImplCommit::None);
    }

    #[test]
    fn card_replies_and_lines() {
        for without in [false, true] {
            assert_eq!(card_reply("1\n", without), CardReply::Start);
            assert_eq!(card_reply(" 2 ", without), CardReply::Change);
            assert_eq!(card_reply("3", without), CardReply::Cancel);
            assert_eq!(card_reply("cancel", without), CardReply::Cancel);
            assert_eq!(card_reply("", without), CardReply::Open);
            assert_eq!(card_reply("y", without), CardReply::Open);
        }
        // Only a card that holds a task runs it without a loop.
        assert_eq!(card_reply("4", false), CardReply::Open);
        assert_eq!(card_reply("4", true), CardReply::RunWithout);
        assert_eq!(
            card_reply("run without a loop", true),
            CardReply::RunWithout
        );
        let card = json!({"attn_id": "01A", "revision": 3, "summary": "api: review loop setup"});
        assert_eq!(
            respond_line(&card, START),
            format!(
                "twr{} respond 01A@3 \"start and save these settings\"",
                workspace_hint()
            )
        );
        assert!(!offers_run_without(&card));
        let held = json!({"attn_id": "01B", "revision": 1,
            "options": [START, RUN_WITHOUT, CANCEL]});
        assert!(offers_run_without(&held));
        let mut r = json!({"status": "needs_setup"});
        add_card(&mut r, held);
        assert!(r["run_without"]
            .as_str()
            .unwrap()
            .ends_with("respond 01B@1 \"run without a loop\""));
        assert!(r["respond"]
            .as_str()
            .unwrap()
            .ends_with("01B@1 \"start and save these settings\""));
        assert_eq!(card_text(&card), "api: review loop setup\n  card: 01A@3");
        assert_eq!(
            answer_params(&card, START),
            json!({"attn_id": "01A", "revision": 3, "answer": START})
        );
    }

    #[test]
    fn loops_read_well() {
        let list = json!([{"thread_name": "api", "kind": "review", "profile": "high",
            "phase": "ladder", "round": "m3", "ledger": 2, "paused": false, "stuck": null,
            "waiting_on": "review"}]);
        assert_eq!(
            list_text(&list),
            "THREAD  KIND    PROFILE  PHASE   ROUND  LEDGER  WAITING ON\n\
             api     review  high     ladder  m3     2       review\n"
        );
        assert_eq!(list_text(&json!([])), "no loop is running\n");
        let detail = json!({
            "thread_name": "api", "id": "L1", "kind": "review", "running": false,
            "outcome": "clean",
            "started": {"settings_from": "saved", "settings_of": "workspace",
                "settings": LoopSettings::first_card(&Default::default()),
                "overrides": {"profile": "low"}, "scope": "branch",
                "bases": [{"repo": "/r", "base": "abc"}]},
            "phases": [{"phase": "scope", "goal": "Harden it.\nMore."}, {"phase": "ladder"}],
            "rounds": [
                {"id": "m1", "effort": "medium", "failed": false,
                 "findings": [{"id": "m1.1", "severity": "major", "location": "src/a.rs:1",
                    "problem": "it breaks\nbadly"}],
                 "triage": {"items": [{"id": "m1.1", "verdict": "dismissed",
                    "reason": "intended"}], "gates": {"ran": ["cargo test"], "result": "pass"}}},
                {"id": "v1", "effort": "medium", "failed": true, "reason": "exit 3"},
            ],
            "ledger": [{"id": "L1", "kind": "dismissed", "from": "m1.1",
                "location": "src/a.rs:1", "problem": "it breaks", "reason": "intended"}],
            "transition": {"reason": "max_rounds", "round": "m1"},
            "ended": {"outcome": "clean"},
            "folder": "/data/loops/L1",
            "report_path": "/data/loops/L1/report.md",
        });
        let text = show_text(&detail);
        for want in [
            "api: review loop L1 (ended: clean)",
            "settings (the workspace's saved settings",
            "  this run     profile low",
            "  scope        branch",
            "  repository   /r (from abc)",
            "goal: Harden it.",
            "  m1 medium: 1 finding",
            "    m1.1 major src/a.rs:1: it breaks — dismissed: intended",
            "    gates: pass (cargo test)",
            "  v1 medium: failed: exit 3",
            "ledger: 1",
            "  L1 (dismissed, m1.1) src/a.rs:1: it breaks — intended",
            "transition: after m1: the last tier ran its profile's rounds",
            "folder: /data/loops/L1",
            "report: /data/loops/L1/report.md",
        ] {
            assert!(text.contains(want), "{want:?} missing:\n{text}");
        }
        let mut skipped = detail.clone();
        skipped["transition"] =
            json!({"reason": "converged", "round": "x1", "verify_skipped": true});
        assert!(show_text(&skipped).contains(
            "transition: after x1: the last tier converged; verification skipped (it could only \
             repeat that round)"
        ));
        // An implementation loop: its task, baseline, plans, implement turns, plan check round,
        // commit checks with their notes, and what it committed and did not.
        let mut s = LoopSettings::first_card(&Default::default());
        s.plan_check = Some(PlanCheck::Always);
        let detail = json!({
            "thread_name": "api", "id": "L2", "kind": "implement", "running": false,
            "outcome": "clean",
            "started": {"settings_from": "setup_item", "settings": s,
                "task": "add a CSV export\nwith a header row", "bases": [{"repo": "/r", "base": "abc"}],
                "baseline": {"r": {"changed": 2, "summary": "2 paths already changed: a, b"}}},
            "plans": [
                {"revision": 1, "contract_risk": true, "reason": "a new endpoint",
                 "path": "/data/loops/L2/plan-1.md"},
                {"revision": 2, "contract_risk": false, "reason": "revised",
                 "path": "/data/loops/L2/plan.md"}],
            "implemented": [
                {"status": "partial", "gates": {"ran": ["cargo test"], "result": "fail"},
                 "accepted": false},
                {"status": "done", "gates": {"ran": ["cargo test"], "result": "pass"},
                 "accepted": true}],
            "rounds": [{"id": "p1", "effort": "medium", "failed": false, "findings": []}],
            "commit_checks": [
                {"problems": ["/r: 1 path is uncommitted beyond the baseline: x.rs"]},
                {"problems": [], "notes": ["the thread wrote no twr-commit block"]}],
            "ended": {"outcome": "clean", "commits": [{"repo": "/r", "sha": "abc1234def"}],
                "commit_problem": "/s: 1 path is uncommitted beyond the baseline: y.rs"},
        });
        let text = show_text(&detail);
        for want in [
            "api: implementation loop L2 (ended: clean)",
            "settings (the settings card you answered",
            "  plan check   always",
            "  commit       end",
            "  task         add a CSV export (+1 more line)",
            "  baseline     2 paths already changed: a, b (out of scope, left out of the commit)",
            "plan 1: contract risk: yes — a new endpoint (/data/loops/L2/plan-1.md)",
            "plan 2: contract risk: no — revised (/data/loops/L2/plan.md)",
            "implement: partial, gates fail (cargo test) — sent back: gates must be green",
            "implement: done, gates pass (cargo test)\n",
            "  p1 medium (plan check): clean",
            "commit check: failed: /r: 1 path is uncommitted beyond the baseline: x.rs",
            "commit check: passed\n  note: the thread wrote no twr-commit block",
            "committed: abc1234def (/r)",
            "not committed: /s: 1 path is uncommitted beyond the baseline: y.rs",
        ] {
            assert!(text.contains(want), "{want:?} missing:\n{text}");
        }
        assert!(!text.contains("scope "), "{text}");
        let started = started_text(&json!({"thread_name": "api", "kind": "implement",
            "line": "low · plan", "started": detail["started"]}));
        assert!(
            started.starts_with("api: implementation loop started (low · plan)"),
            "{started}"
        );
        assert!(started.contains("loop implement api --setup"), "{started}");
        assert!(
            started.contains("  task         add a CSV export"),
            "{started}"
        );
        assert_eq!(task_line(&"x".repeat(150)), format!("{}…", "x".repeat(100)));
        assert_eq!(
            control_text(
                "pause",
                &json!({"thread_name": "api", "running": true,
                "line": "high · ladder m3 · paused"})
            ),
            "api's loop paused (high · ladder m3 · paused)"
        );
        assert_eq!(
            control_text(
                "stop",
                &json!({"thread_name": "api", "running": false,
                "last": {"outcome": "stopped"}})
            ),
            "api's loop ended: stopped"
        );
    }
    /// `twr loop show` lists a sweep audited by runs: each rule, its audit and the thread's
    /// verdict; nothing for a sweep the thread audited itself.
    #[test]
    fn show_lists_the_sweeps_audits() {
        let d = json!({"sweep": {"mode": "runs", "rules": [
            {"name": "identity-gating", "rule": "Every handler checks the caller.", "where": "src/**",
             "audit": {"id": "s1", "status": "answered", "count": 1, "failures": [],
                       "findings": [{"id": "s1.1", "severity": "major", "location": "a.rs:3",
                                     "problem": "no check"}]},
             "verdict": {"rule": "identity-gating", "result": "fixed"}},
            {"name": "retries", "rule": "r", "where": "w",
             "audit": {"id": "s2", "status": "not_run", "count": 0,
                       "failures": ["exit 1", "timed out"], "findings": []},
             "verdict": {"rule": "retries", "result": "not_audited", "reason": "no time"}},
            {"name": "locks", "rule": "r", "where": "w",
             "audit": {"id": "s3", "status": "running", "count": 0, "failures": ["exit 1"],
                       "findings": []}}
        ]}});
        assert_eq!(
            sweep_rows(&d),
            [
                "sweep (audited by runs):",
                "  s1 identity-gating: 1 finding → fixed",
                "    rule: Every handler checks the caller. (where: src/**)",
                "    s1.1 major a.rs:3: no check",
                "  s2 retries: not run: timed out → not audited (no time)",
                "    rule: r (where: w)",
                "  s3 locks: running (retry)",
                "    rule: r (where: w)",
            ]
        );
        assert!(sweep_rows(&json!({"sweep": {"mode": "thread", "rules": null}})).is_empty());
        assert_eq!(
            sweep_rows(&json!({"sweep": {"rules": []}})),
            ["sweep (audited by runs): no rule named"]
        );
    }
}

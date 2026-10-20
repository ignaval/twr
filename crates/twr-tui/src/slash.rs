//! Slash commands of the input line: `/open`, `/back`, `/unsend`, `/mode`, `/model`,
//! `/account`, `/accounts`, `/rename`, `/orch`, `/new`, `/park`, `/resume`, `/compact-context`,
//! `/stop`, `/pin`, `/delete`, `/trash`, `/attach`,
//! `/parked`, `/sidebar`, `/spaces`, `/focus`, `/grid`, `/decisions`, `/mouse`, `/ws`,
//! `/settings`, `/help`, `/quit`. Pure parsing, the
//! suggestions of the popup above the input line ([`suggest`]) and Tab completion;
//! [`crate::app`] turns a parsed command into actions, confirmations or a status line. The same
//! popup completes an `@` mention of a worker thread anywhere in a message ([`mention`]).
//!
//! A line that starts with `/` is a command and never a message: an unknown one is an error
//! in the status bar. `//` at the start says the rest as a message (`//tmp is full` says
//! `/tmp is full`).

/// One parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    /// `/clear`: reset the orchestrator without carrying the conversation over.
    Clear,
    /// `/open [thread]`: talk to a worker thread directly (dive in); default: the selected card.
    Open(Option<String>),
    /// `/back`: back to the orchestrator conversation.
    Back,
    /// `/unsend`: take back the last message sent, while it still can be (as Esc twice on an
    /// empty line).
    Unsend,
    /// `/mode [thread] <mode>`: set a thread's permission mode (in a dive the thread may be
    /// left out).
    Mode {
        /// The thread; `None` = the one dived into.
        thread: Option<String>,
        /// The mode id.
        mode: String,
    },
    /// `/model <thread> [model] [--effort e] [--runtime r]`: show a thread's model and
    /// reasoning effort, or set either one (`default` or blank: the runtime's default; the hub
    /// checks the effort), or move the thread to another runtime. The first word is always the
    /// thread, as in `twr model`; in a dive it may be left out when no model is given
    /// (`/model`, `/model --effort high`).
    Model {
        /// The thread; `None` = the one dived into.
        thread: Option<String>,
        /// The model, as typed; `None` keeps it (on another runtime: that runtime's default).
        model: Option<String>,
        /// The reasoning effort, as typed; `None` keeps it (on another runtime: that
        /// runtime's default).
        effort: Option<String>,
        /// The runtime to move the thread to, as typed (the hub checks it); `None` keeps it.
        runtime: Option<String>,
    },
    /// `/account [thread] [name] [--outside-pin]`: show the account a thread runs on, or move
    /// it to account `name` (`thread.set_model {account}`: a new session with a handoff, even
    /// on the same runtime; `--outside-pin` leaves the thread's or its project's pin). One
    /// word is a thread, but in a dive the account to move to unless it names a thread and no
    /// account of the dived thread's runtime ([`crate::app`] decides).
    Account {
        /// The thread; `None` = the one dived into, else the selected one.
        thread: Option<String>,
        /// The account to move to; `None` shows the thread's.
        account: Option<String>,
        /// Leave the thread's or its project's pin (`outside_pin`).
        outside_pin: bool,
    },
    /// `/account add <runtime> <name>`: the `twr account add` command to run in another
    /// terminal (its login needs the terminal).
    AccountAdd {
        /// The runtime, as typed.
        runtime: String,
        /// The account's name, as typed.
        name: String,
    },
    /// `/accounts`: every runtime's accounts and their logins.
    Accounts,
    /// `/rename [thread] <name>`: give a worker thread a new name (in a dive the thread may
    /// be left out).
    Rename {
        /// The thread; `None` = the one dived into.
        thread: Option<String>,
        /// The new name, as typed (the hub checks it).
        name: String,
    },
    /// `/orch <runtime> [model]` (alias `/orchestrator`): switch the orchestrator.
    Orch {
        /// Runtime.
        runtime: String,
        /// Model; `None` = the runtime's default.
        model: Option<String>,
    },
    /// `/orch` without arguments: pick the runtime to switch the orchestrator to.
    OrchPick,
    /// `/new <project> [--runtime r] [--name n] [--account a [--outside-pin]]`: a new
    /// thread.
    New {
        /// A configured project name or a path.
        project: String,
        /// Runtime (default: the project's).
        runtime: Option<String>,
        /// Thread name (default: derived from the project).
        name: Option<String>,
        /// The account (a name, a glob or a comma-separated list; default: the project's pin,
        /// else the runtime's default account).
        account: Option<String>,
        /// Open it on `account` although the project's pin leaves it out (`outside_pin`).
        outside_pin: bool,
    },
    /// `/park [thread]`.
    Park(Option<String>),
    /// `/resume [thread]`.
    Resume(Option<String>),
    /// `/trim [thread]`: delete the build output of a thread's worktree (`thread.trim`).
    Trim(Option<String>),
    /// `/compact-context [thread]`: compact a thread's context (`thread.compact`, what the
    /// agent's own `/compact` does; `/compact` is the TUI's phone layout).
    CompactContext(Option<String>),
    /// `/stop [thread]`.
    Stop(Option<String>),
    /// `/pin [thread]`: pin or unpin a thread's card (default: the selected one).
    Pin(Option<String>),
    /// `/delete <thread>...`: ask the hub to delete one or more threads (`thread.delete`); it
    /// raises one delete card for all of them, and nothing goes until the user answers Delete
    /// on it; then they go to the trash.
    Delete(Vec<String>),
    /// `/trash ...`: list the trash, purge items now, or restore a deleted thread.
    Trash(TrashCmd),
    /// `/attach [thread]`.
    Attach(Option<String>),
    /// `/parked [on|off]`: list parked threads among the cards or hide them; `None` toggles.
    Parked(Option<bool>),
    /// `/sidebar [on|off]`: show or hide worker cards; `None` toggles.
    Sidebar(Option<bool>),
    /// `/compact [on|off]`: the compact layout, one screen at a time; `None` toggles.
    Compact(Option<bool>),
    /// `/c`, `/n`, `/t`: the compact layout's screen.
    Pane(crate::app::CompactPane),
    /// `/focus [on|off]`: list working threads and threads needing attention; `None` toggles.
    Focus(Option<bool>),
    /// `/grid`: the grid view, or back to the stack view (as Ctrl-G).
    Grid,
    /// `/decisions`: open the decisions overlay, or close it.
    Decisions,
    /// `/mouse [on|off]`: take the mouse (select and copy in the TUI) or leave it to the
    /// terminal; `None` toggles.
    Mouse(Option<bool>),
    /// `/suggest [on|off]`: next-message suggestions for this TUI ([`crate::suggest`]).
    Suggest(Option<bool>),
    /// `/summaries [on|off]`: summaries of the threads' routine updates for this TUI
    /// ([`crate::summary`]).
    Summaries(Option<bool>),
    /// `/ws [name]` (alias `/workspace`): list the workspaces, or switch to one (a new name
    /// asks to create it).
    Ws(Option<String>),
    /// `/ws new [name]`: create a workspace and switch to it; without a name, ask for it
    /// (the new-workspace dialog, [`crate::sidebar`]).
    WsNew(Option<String>),
    /// `/spaces [on|off]`: show or hide the workspace sidebar (Ctrl-T); `None` toggles.
    Spaces(Option<bool>),
    /// `/settings`: the settings overlay ([`crate::settings`]).
    Settings,
    /// `/help`.
    Help,
    /// `/quit`: leave the TUI (as Ctrl-C).
    Quit,
}

/// What `/trash` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashCmd {
    /// `/trash` (or `/trash list`, `/trash ls`): show the trash (`trash.list`).
    List,
    /// `/trash empty <item>...` or `/trash empty --all`: purge items now (`trash.empty`,
    /// asks y first).
    Empty {
        /// Items as typed (an id, a unique end of one, or a thread name).
        items: Vec<String>,
        /// Every item.
        all: bool,
    },
    /// `/trash restore <item> [as <name>]`: bring a deleted thread back, copied from the item,
    /// which stays until purged (`trash.restore`).
    Restore {
        /// The item as typed.
        item: String,
        /// The name to restore it under.
        as_name: Option<String>,
    },
}

/// What an argument of a command completes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// A runtime that can host the orchestrator.
    Runtime,
    /// A thread name.
    Thread,
    /// A thread name, as is every argument after it.
    Threads,
    /// A permission mode.
    Mode,
    /// `/mode`'s first argument: a mode in a dive (then a thread name), else a thread name.
    ModeOrThread,
    /// `on` or `off`.
    OnOff,
    /// A workspace name.
    Workspace,
    /// An account of the thread named before it (else of the thread dived into or selected).
    Account,
    /// `/account`'s first argument: a thread name, and in a dive an account too.
    AccountOrThread,
    /// Nothing to complete.
    Free,
}

/// Every command: name, usage, what it does, what its arguments complete to (by position).
pub const COMMANDS: &[(&str, &str, &str, &[ArgKind])] = &[
    (
        "/open",
        "/open [thread]",
        "talk to a worker directly (default: the selected card)",
        &[ArgKind::Thread],
    ),
    (
        "/back",
        "/back",
        "back to the orchestrator conversation (like Esc in a dive)",
        &[],
    ),
    (
        "/unsend",
        "/unsend",
        "take back your last message while nothing was done with it yet (like Esc twice)",
        &[],
    ),
    (
        "/mode",
        "/mode [thread] <mode>",
        "set a permission mode (in a dive: /mode <mode>)",
        &[ArgKind::ModeOrThread, ArgKind::Mode],
    ),
    (
        "/model",
        "/model <thread> [model] [--effort e] [--runtime r]",
        "show or set a model and reasoning effort, or move to another runtime (in a dive: \
         /model [--effort e])",
        &[ArgKind::Thread],
    ),
    (
        "/account",
        "/account [thread] [name] [--outside-pin] | add <runtime> <name>",
        "show the account a thread runs on, or move it to another (a new session with a \
         handoff; in a dive: /account [name]); add: the command that adds one",
        &[ArgKind::AccountOrThread, ArgKind::Account],
    ),
    (
        "/accounts",
        "/accounts",
        "every runtime's accounts and their logins",
        &[],
    ),
    (
        "/rename",
        "/rename [thread] <name>",
        "rename a worker thread; its worktree and branch keep theirs (in a dive: /rename <name>)",
        &[ArgKind::Thread, ArgKind::Free],
    ),
    (
        "/orch",
        "/orch [<runtime> [model]]",
        "switch the orchestrator (y: with a summary, s: without); alone, pick its runtime; \
         alias /orchestrator",
        &[ArgKind::Runtime],
    ),
    (
        "/clear",
        "/clear",
        "fresh orchestrator session, no handoff (asks y; history stays in the log)",
        &[],
    ),
    (
        "/new",
        "/new <project> [--runtime r] [--name n] [--account a [--outside-pin]]",
        "start a thread in a project (a configured name or a path)",
        &[],
    ),
    (
        "/park",
        "/park [thread]",
        "park a thread (default: the selected one; asks y)",
        &[ArgKind::Thread],
    ),
    (
        "/resume",
        "/resume [thread]",
        "resume a parked thread",
        &[ArgKind::Thread],
    ),
    (
        "/trim",
        "/trim [thread]",
        "delete an idle thread's build output (target/, node_modules/, ...)",
        &[ArgKind::Thread],
    ),
    (
        "/compact-context",
        "/compact-context [thread]",
        "compact a thread's context, as its agent's own /compact does (in a dive: that thread)",
        &[ArgKind::Thread],
    ),
    (
        "/stop",
        "/stop [thread]",
        "stop a thread (asks y)",
        &[ArgKind::Thread],
    ),
    (
        "/pin",
        "/pin [thread]",
        "pin or unpin a thread's card (default: the selected one)",
        &[ArgKind::Thread],
    ),
    (
        "/delete",
        "/delete <thread>...",
        "delete threads: their worktrees and branches go to the trash (asks on a card)",
        &[ArgKind::Threads],
    ),
    (
        "/trash",
        "/trash [empty <item>...|--all] [restore <item> [as <name>]]",
        "show the trash, purge items now (asks y) or restore a deleted thread (copied)",
        &[ArgKind::Free],
    ),
    (
        "/attach",
        "/attach [thread]",
        "show the `twr attach` command to run in another terminal",
        &[ArgKind::Thread],
    ),
    (
        "/parked",
        "/parked [on|off]",
        "list parked threads among the cards, or hide them",
        &[ArgKind::OnOff],
    ),
    (
        "/sidebar",
        "/sidebar [on|off]",
        "show or hide the thread sidebar (Ctrl-B); returns to the conversation",
        &[ArgKind::OnOff],
    ),
    (
        "/spaces",
        "/spaces [on|off]",
        "show or hide the workspace sidebar (Ctrl-T): each workspace's orchestrator, new \
         workspace, settings",
        &[ArgKind::OnOff],
    ),
    (
        "/compact",
        "/compact [on|off]",
        "the phone layout: one screen at a time (on by itself below [tui] compact_width columns)",
        &[ArgKind::OnOff],
    ),
    ("/c", "/c", "compact layout: the conversation", &[]),
    (
        "/n",
        "/n",
        "compact layout: what needs you (Enter opens an item)",
        &[],
    ),
    ("/t", "/t", "compact layout: the threads", &[]),
    ("/w", "/w", "compact layout: the workspaces", &[]),
    (
        "/focus",
        "/focus [on|off]",
        "Focus mode: working threads and anything needing you (Ctrl-F)",
        &[ArgKind::OnOff],
    ),
    (
        "/grid",
        "/grid",
        "the grid of threads, or back to the stack view (Ctrl-G)",
        &[],
    ),
    (
        "/decisions",
        "/decisions",
        "the decision audit view (Esc closes it)",
        &[],
    ),
    (
        "/mouse",
        "/mouse [on|off]",
        "mouse selection and wheel in the TUI, or the terminal's own",
        &[ArgKind::OnOff],
    ),
    (
        "/suggest",
        "/suggest [on|off]",
        "suggest your next message after a turn (a small model; Tab takes it)",
        &[ArgKind::OnOff],
    ),
    (
        "/summaries",
        "/summaries [on|off]",
        "a line on where a thread stands in its folded routine updates (a small model)",
        &[ArgKind::OnOff],
    ),
    (
        "/ws",
        "/ws [name] | /ws new [name]",
        "list the workspaces, or switch to one (Alt-1..9, Ctrl-PgUp/PgDn); a new name asks to \
         create it; /ws new creates one; alias /workspace",
        &[ArgKind::Workspace],
    ),
    (
        "/settings",
        "/settings",
        "the settings: suggestions, card order, notifications, compact width, ...",
        &[],
    ),
    (
        "/help",
        "/help",
        "this help (↑↓ scroll it, Esc closes it; ? on an empty line opens it too)",
        &[],
    ),
    ("/quit", "/quit", "leave the TUI (Ctrl-C)", &[]),
];

/// twr's permission modes, as `thread.set_mode` takes them on every runtime (the hub's
/// `SHARED_MODES`; a runtime may take ids of its own too, which `/mode` passes on as typed).
pub const MODES: &[&str] = &[
    "manual",
    "default",
    "plan",
    "read-only",
    "dontAsk",
    "acceptEdits",
    "auto",
    "bypassPermissions",
];

/// Whether command `name` (canonical) takes arguments at all.
pub fn takes_args(name: &str) -> bool {
    COMMANDS
        .iter()
        .find(|c| c.0 == name)
        .is_some_and(|c| c.1.contains(['<', '[']))
}

/// Aliases: `(alias, command)`.
const ALIASES: &[(&str, &str)] = &[("/orchestrator", "/orch"), ("/workspace", "/ws")];

/// Byte offset in `s` just after its last whitespace character, whatever its UTF-8 width
/// (U+2003, U+00A0...): where the last word starts. `None` when `s` has no whitespace.
fn after_last_space(s: &str) -> Option<usize> {
    s.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
}

/// Splits `line` into words: whitespace separates, `'...'` and `"..."` quote (no escapes).
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.get_or_insert_with(String::new).push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                cur.get_or_insert_with(String::new);
            }
            None if c.is_whitespace() => {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
            }
            None => cur.get_or_insert_with(String::new).push(c),
        }
    }
    if quote.is_some() {
        return Err("unclosed quote".into());
    }
    out.extend(cur);
    Ok(out)
}

/// The command `name` names (an alias resolved), if any.
pub fn canonical(name: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .map(|c| c.0)
        .find(|c| *c == name)
        .or_else(|| ALIASES.iter().find(|a| a.0 == name).map(|a| a.1))
}

/// Parses a line that starts with `/` (not `//`). The error is for the status bar.
pub fn parse(line: &str) -> Result<Slash, String> {
    let words = words(line.trim())?;
    let Some((name, args)) = words.split_first() else {
        return Err("empty command; /help lists them".into());
    };
    let Some(cmd) = canonical(name) else {
        return Err(format!(
            "unknown command {name} (/help lists them; start with // to say a line that \
             begins with /)"
        ));
    };
    let usage = COMMANDS.iter().find(|c| c.0 == cmd).map_or("", |c| c.1);
    let bad = || Err(format!("usage: {usage}"));
    let one = |args: &[String]| match args {
        [] => Ok(None),
        [t] => Ok(Some(t.clone())),
        _ => Err(format!("usage: {usage}")),
    };
    match cmd {
        "/clear" if args.is_empty() => Ok(Slash::Clear),
        "/open" => one(args).map(Slash::Open),
        "/back" if args.is_empty() => Ok(Slash::Back),
        "/unsend" if args.is_empty() => Ok(Slash::Unsend),
        "/mode" => match args {
            [mode] => Ok(Slash::Mode {
                thread: None,
                mode: mode.clone(),
            }),
            [thread, mode] => Ok(Slash::Mode {
                thread: Some(thread.clone()),
                mode: mode.clone(),
            }),
            _ => bad(),
        },
        "/rename" => match args {
            [name] => Ok(Slash::Rename {
                thread: None,
                name: name.clone(),
            }),
            [thread, name] => Ok(Slash::Rename {
                thread: Some(thread.clone()),
                name: name.clone(),
            }),
            _ => bad(),
        },
        "/model" => {
            let (mut words, mut effort, mut runtime) = (Vec::new(), None, None);
            let mut it = args.iter();
            while let Some(a) = it.next() {
                let (flag, inline) = match a.split_once('=') {
                    Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                    _ => (a.as_str(), None),
                };
                let slot = match flag {
                    "--effort" => &mut effort,
                    "--runtime" => &mut runtime,
                    f if f.starts_with("--") => {
                        return Err(format!("unknown option {f}; usage: {usage}"))
                    }
                    _ => {
                        words.push(a.clone());
                        continue;
                    }
                };
                let value = match inline.or_else(|| it.next().cloned()) {
                    Some(v) if slot.is_none() => v,
                    _ => return bad(),
                };
                *slot = Some(value);
            }
            let (thread, model) = match words.as_slice() {
                [] => (None, None),
                [thread] => (Some(thread.clone()), None),
                [thread, model] => (Some(thread.clone()), Some(model.clone())),
                _ => return bad(),
            };
            Ok(Slash::Model {
                thread,
                model,
                effort,
                runtime,
            })
        }
        "/orch" => match args {
            [] => Ok(Slash::OrchPick),
            [runtime] => Ok(Slash::Orch {
                runtime: runtime.clone(),
                model: None,
            }),
            [runtime, model] => Ok(Slash::Orch {
                runtime: runtime.clone(),
                model: Some(model.clone()),
            }),
            _ => bad(),
        },
        "/account" => {
            if let [add, runtime, name] = args {
                if add == "add" {
                    return Ok(Slash::AccountAdd {
                        runtime: runtime.clone(),
                        name: name.clone(),
                    });
                }
            }
            let mut outside_pin = false;
            let mut words = Vec::new();
            for a in args {
                match a.as_str() {
                    "--outside-pin" => outside_pin = true,
                    f if f.starts_with("--") => {
                        return Err(format!("unknown option {f}; usage: {usage}"))
                    }
                    _ => words.push(a.clone()),
                }
            }
            let (thread, account) = match words.as_slice() {
                [] => (None, None),
                [one] => (Some(one.clone()), None),
                [thread, account] => (Some(thread.clone()), Some(account.clone())),
                _ => return bad(),
            };
            // Leaving a pin is for a move; in a dive one word is the account.
            if outside_pin && account.is_none() && thread.is_none() {
                return bad();
            }
            Ok(Slash::Account {
                thread,
                account,
                outside_pin,
            })
        }
        "/accounts" if args.is_empty() => Ok(Slash::Accounts),
        "/new" => {
            let mut project = None;
            let (mut runtime, mut name, mut account) = (None, None, None);
            let mut outside_pin = false;
            let mut it = args.iter();
            while let Some(a) = it.next() {
                // `--account=work` as `--account work`.
                let (flag, inline) = match a.split_once('=') {
                    Some((f, v)) if f.starts_with("--") => (f, Some(v)),
                    _ => (a.as_str(), None),
                };
                let slot = match flag {
                    "--runtime" => &mut runtime,
                    "--name" => &mut name,
                    "--account" => &mut account,
                    "--outside-pin" if inline.is_none() => {
                        outside_pin = true;
                        continue;
                    }
                    "--outside-pin" => return bad(),
                    flag if flag.starts_with("--") => {
                        return Err(format!("unknown option {flag}; usage: {usage}"))
                    }
                    _ if project.is_none() => {
                        project = Some(a.clone());
                        continue;
                    }
                    _ => return bad(),
                };
                match inline.map(str::to_string).or_else(|| it.next().cloned()) {
                    Some(v) if slot.is_none() => *slot = Some(v),
                    _ => return bad(),
                }
            }
            match project {
                // Leaving the pin is for an account given (`--account`, or the account part
                // of `--runtime <runtime>:<account>`).
                Some(_)
                    if outside_pin
                        && account.is_none()
                        && !runtime
                            .as_deref()
                            .is_some_and(|r| twr_core::account::split_runtime(r).1.is_some()) =>
                {
                    bad()
                }
                Some(project) => Ok(Slash::New {
                    project,
                    runtime,
                    name,
                    account,
                    outside_pin,
                }),
                None => bad(),
            }
        }
        "/park" => one(args).map(Slash::Park),
        "/resume" => one(args).map(Slash::Resume),
        "/trim" => one(args).map(Slash::Trim),
        "/compact-context" => one(args).map(Slash::CompactContext),
        "/stop" => one(args).map(Slash::Stop),
        "/pin" => one(args).map(Slash::Pin),
        "/delete" => match args {
            [] => bad(),
            threads => Ok(Slash::Delete(threads.to_vec())),
        },
        "/trash" => {
            let words: Vec<&str> = args.iter().map(String::as_str).collect();
            match words.as_slice() {
                [] | ["list" | "ls"] => Ok(Slash::Trash(TrashCmd::List)),
                ["empty", "--all"] => Ok(Slash::Trash(TrashCmd::Empty {
                    items: Vec::new(),
                    all: true,
                })),
                ["empty", items @ ..] if !items.is_empty() && !items.contains(&"--all") => {
                    Ok(Slash::Trash(TrashCmd::Empty {
                        items: items.iter().map(|i| i.to_string()).collect(),
                        all: false,
                    }))
                }
                ["restore", item] => Ok(Slash::Trash(TrashCmd::Restore {
                    item: item.to_string(),
                    as_name: None,
                })),
                ["restore", item, "as" | "--as", name] => Ok(Slash::Trash(TrashCmd::Restore {
                    item: item.to_string(),
                    as_name: Some(name.to_string()),
                })),
                _ => bad(),
            }
        }
        "/attach" => one(args).map(Slash::Attach),
        "/ws" => match args {
            [new] if new == "new" => Ok(Slash::WsNew(None)),
            [new, name] if new == "new" => Ok(Slash::WsNew(Some(name.clone()))),
            _ => one(args).map(Slash::Ws),
        },
        "/parked" | "/sidebar" | "/spaces" | "/focus" | "/mouse" | "/compact" | "/suggest"
        | "/summaries" => {
            let on = match one(args)?.as_deref() {
                None => None,
                Some("on") => Some(true),
                Some("off") => Some(false),
                Some(_) => return bad(),
            };
            Ok(match cmd {
                "/parked" => Slash::Parked(on),
                "/sidebar" => Slash::Sidebar(on),
                "/spaces" => Slash::Spaces(on),
                "/focus" => Slash::Focus(on),
                "/compact" => Slash::Compact(on),
                "/suggest" => Slash::Suggest(on),
                "/summaries" => Slash::Summaries(on),
                _ => Slash::Mouse(on),
            })
        }
        "/help" if args.is_empty() => Ok(Slash::Help),
        "/grid" if args.is_empty() => Ok(Slash::Grid),
        "/c" if args.is_empty() => Ok(Slash::Pane(crate::app::CompactPane::Chat)),
        "/n" if args.is_empty() => Ok(Slash::Pane(crate::app::CompactPane::Needs)),
        "/t" if args.is_empty() => Ok(Slash::Pane(crate::app::CompactPane::Threads)),
        "/w" if args.is_empty() => Ok(Slash::Pane(crate::app::CompactPane::Workspaces)),
        "/settings" if args.is_empty() => Ok(Slash::Settings),
        "/decisions" if args.is_empty() => Ok(Slash::Decisions),
        "/quit" if args.is_empty() => Ok(Slash::Quit),
        _ => bad(),
    }
}

/// Tab completion of `input` (a line starting with `/`): a command name, the runtime of
/// `/orch`, or a thread name. Returns the new line when it can extend it, and the candidates
/// when there are several (for the status bar).
pub fn complete(
    input: &str,
    runtimes: &[String],
    threads: &[String],
    workspaces: &[String],
) -> (Option<String>, Vec<String>) {
    let (head, last) = match after_last_space(input) {
        Some(at) => input.split_at(at),
        None => ("", input),
    };
    let candidates: Vec<String> = if head.is_empty() {
        COMMANDS
            .iter()
            .map(|c| c.0)
            .chain(ALIASES.iter().map(|a| a.0))
            .filter(|c| c.starts_with(last))
            .map(str::to_string)
            .collect()
    } else {
        // Only the first argument completes, or any of a list of threads.
        let words: Vec<&str> = head.split_whitespace().collect();
        let first = words
            .first()
            .and_then(|cmd| canonical(cmd))
            .and_then(|c| COMMANDS.iter().find(|x| x.0 == c))
            .and_then(|c| c.3.first().copied())
            .unwrap_or(ArgKind::Free);
        let kind = match (words.len(), first) {
            (1, k) | (_, k @ ArgKind::Threads) => k,
            _ => ArgKind::Free,
        };
        let modes: Vec<String> = MODES.iter().map(|m| m.to_string()).collect();
        let on_off = ["on".to_string(), "off".into()];
        let pool = match kind {
            ArgKind::Runtime => runtimes,
            ArgKind::Thread
            | ArgKind::Threads
            | ArgKind::ModeOrThread
            | ArgKind::AccountOrThread => threads,
            ArgKind::Mode => &modes[..],
            ArgKind::OnOff => &on_off[..],
            ArgKind::Workspace => workspaces,
            ArgKind::Account | ArgKind::Free => &[][..],
        };
        pool.iter()
            .filter(|c| c.starts_with(last))
            .cloned()
            .collect()
    };
    match candidates.as_slice() {
        [] => (None, Vec::new()),
        [one] => (Some(format!("{head}{one} ")), Vec::new()),
        many => {
            let common = many[1..].iter().fold(many[0].clone(), |acc, c| {
                acc.chars()
                    .zip(c.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect()
            });
            let line = (common.len() > last.len()).then(|| format!("{head}{common}"));
            (line, many.to_vec())
        }
    }
}

/// One row of the suggestion popup above the input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// What replaces the word being typed.
    pub value: String,
    /// What the row shows first: a command's usage, else the value.
    pub label: String,
    /// What it shows after (dimmed): a command's description, a thread's status.
    pub detail: String,
    /// Typing goes on after it, so completing it adds a space: a command that takes
    /// arguments, or an argument.
    pub more: bool,
}

/// The popup's rows for a `/` line or an `@` mention, and where the word they complete is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Suggestions {
    /// Byte offset in the line of the word the rows replace.
    pub start: usize,
    /// Byte offset in the line where the text the rows replace ends: the end of a `/` line,
    /// the end of the mention's word for a mention (the next whitespace after the cursor, or
    /// the end of the line), so completing inside a word leaves none of it behind.
    pub end: usize,
    /// The rows are command names (else values of an argument, or mentions).
    pub command: bool,
    /// The rows are `@` mentions of threads ([`mention`]): Enter puts the picked one in
    /// instead of sending the line.
    pub mention: bool,
    /// The matching rows, best first: for a `/` line those that start with the word, then
    /// those that contain it; for a mention see [`mention`].
    pub items: Vec<Suggestion>,
}

/// What arguments complete to, each with the detail its row shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pools {
    /// Runtimes that can host the orchestrator (`/orch`).
    pub runtimes: Vec<(String, String)>,
    /// Thread names.
    pub threads: Vec<(String, String)>,
    /// In a dive, `/mode`'s first argument is the mode.
    pub dive: bool,
    /// Workspace names (`/ws`).
    pub workspaces: Vec<(String, String)>,
    /// Each runtime's accounts that are not removed, with their state (`/account`:
    /// `work`, `logged in`).
    pub accounts: std::collections::BTreeMap<String, Vec<(String, String)>>,
    /// Each thread's runtime, by name: whose accounts `/account <thread>` completes.
    pub thread_runtimes: std::collections::BTreeMap<String, String>,
    /// The runtime of the thread dived into, else of the selected one: whose accounts
    /// `/account` completes when no thread is named.
    pub account_runtime: Option<String>,
    /// Each configured project's runtime as far as the TUI knows it, by project name: the
    /// runtime of the latest thread made for it (`/new <project> --account` completes its
    /// accounts).
    pub project_runtimes: std::collections::BTreeMap<String, String>,
    /// The hub's `default_runtime`, when the TUI knows it (`/new --account` without a
    /// runtime or a project it knows).
    pub default_runtime: Option<String>,
    /// The threads an `@` mention can name: workers only, never an orchestrator (current or
    /// retired) or the hub, which are no relay targets.
    pub mentions: Vec<Mentionable>,
}

/// A worker thread an `@` mention can name ([`mention`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mentionable {
    /// The thread's name: what the mention puts in, exact, so that `@name` resolves under the
    /// strict command grammar.
    pub name: String,
    /// Its aliases: what is typed matches them too, but the name goes in.
    pub aliases: Vec<String>,
    /// What its row shows after the name (status, runtime, mode).
    pub detail: String,
    /// Not stopped: listed before the stopped ones.
    pub live: bool,
}

/// `rows` whose keys match `word` (case does not matter): those with a key that starts with
/// it first, then those with a key that contains it, each group in the given order.
fn ranked<T>(word: &str, rows: Vec<(Vec<String>, T)>) -> Vec<T> {
    let word = word.to_lowercase();
    let (mut prefix, mut inner) = (Vec::new(), Vec::new());
    for (keys, row) in rows {
        let keys: Vec<String> = keys.iter().map(|k| k.to_lowercase()).collect();
        if keys.iter().any(|k| k.starts_with(&word)) {
            prefix.push(row);
        } else if keys.iter().any(|k| k.contains(&word)) {
            inner.push(row);
        }
    }
    prefix.extend(inner);
    prefix
}

/// The suggestions for `line` while it is typed: command names while the first word is
/// typed, then the values of the argument under way (thread names, modes, runtimes,
/// `on`/`off`). Empty for a line that is not a command (no `/`, or `//`), an unknown command,
/// or an argument with nothing to offer.
pub fn suggest(line: &str, pools: &Pools) -> Suggestions {
    if !line.starts_with('/') || line.starts_with("//") {
        return Suggestions::default();
    }
    let Some(start) = after_last_space(line) else {
        let word = line.trim_start_matches('/');
        let rows = COMMANDS
            .iter()
            .map(|c| {
                let mut keys = vec![c.0.trim_start_matches('/').to_string()];
                keys.extend(
                    ALIASES
                        .iter()
                        .filter(|a| a.1 == c.0)
                        .map(|a| a.0.trim_start_matches('/').to_string()),
                );
                let row = Suggestion {
                    value: c.0.to_string(),
                    label: c.1.to_string(),
                    detail: c.2.to_string(),
                    more: takes_args(c.0),
                };
                (keys, row)
            })
            .collect();
        return Suggestions {
            start: 0,
            end: line.len(),
            command: true,
            mention: false,
            items: ranked(word, rows),
        };
    };
    let (head, word) = line.split_at(start);
    let words: Vec<&str> = head.split_whitespace().collect();
    let Some(cmd) = words.first().and_then(|w| canonical(w)) else {
        return Suggestions::default();
    };
    // `/new <project> --account <name>` (or `--account=<name>`): the accounts of the runtime
    // the line names.
    if cmd == "/new" {
        let inline = word.strip_prefix("--account=");
        if inline.is_some() || (words.last() == Some(&"--account") && !word.starts_with("--")) {
            let (start, word) = match inline {
                Some(rest) => (line.len() - rest.len(), rest),
                None => (start, word),
            };
            let rows = new_accounts(&words[1..], pools)
                .into_iter()
                .map(|(value, detail)| {
                    let row = Suggestion {
                        label: value.clone(),
                        value: value.clone(),
                        detail,
                        more: true,
                    };
                    (vec![value], row)
                })
                .collect();
            return Suggestions {
                start,
                end: line.len(),
                command: false,
                mention: false,
                items: ranked(word, rows),
            };
        }
    }
    let kinds = COMMANDS
        .iter()
        .find(|c| c.0 == cmd)
        .map_or(&[][..], |c| c.3);
    let modes = || {
        MODES
            .iter()
            .map(|m| (m.to_string(), "permission mode".to_string()))
            .collect::<Vec<_>>()
    };
    // A list of threads goes on to the end of the line.
    let kind = kinds
        .get(words.len() - 1)
        .or_else(|| kinds.last().filter(|k| **k == ArgKind::Threads));
    let pool: Vec<(String, String)> = match kind {
        Some(ArgKind::Runtime) => pools.runtimes.clone(),
        Some(ArgKind::Thread | ArgKind::Threads) => pools.threads.clone(),
        Some(ArgKind::Mode) => modes(),
        Some(ArgKind::ModeOrThread) if pools.dive => {
            let mut all = modes();
            all.extend(pools.threads.iter().cloned());
            all
        }
        Some(ArgKind::ModeOrThread) => pools.threads.clone(),
        Some(ArgKind::OnOff) if cmd == "/parked" => vec![
            ("on".into(), "parked threads are listed".into()),
            ("off".into(), "parked threads are hidden".into()),
        ],
        Some(ArgKind::OnOff) if cmd == "/sidebar" => vec![
            ("on".into(), "thread sidebar is shown".into()),
            ("off".into(), "conversation uses the full area".into()),
        ],
        Some(ArgKind::OnOff) if cmd == "/compact" => vec![
            ("on".into(), "one screen at a time".into()),
            ("off".into(), "the full layout".into()),
        ],
        Some(ArgKind::OnOff) if cmd == "/focus" => vec![
            (
                "on".into(),
                "working threads and anything needing you".into(),
            ),
            ("off".into(), "all unparked threads are listed".into()),
        ],
        Some(ArgKind::OnOff) => vec![
            ("on".into(), "the TUI selects and copies".into()),
            ("off".into(), "the terminal selects".into()),
        ],
        Some(ArgKind::Workspace) => pools.workspaces.clone(),
        // `/account add <runtime> <name>`: the runtimes that take accounts, then a new name.
        Some(ArgKind::Account) if words.get(1) == Some(&"add") => pools
            .accounts
            .keys()
            .map(|r| (r.clone(), "runtime".to_string()))
            .collect(),
        Some(ArgKind::Account) => {
            let runtime = words
                .get(1)
                .and_then(|w| pools.thread_runtimes.get(*w))
                .or(pools.account_runtime.as_ref());
            runtime
                .and_then(|r| pools.accounts.get(r))
                .cloned()
                .unwrap_or_default()
        }
        Some(ArgKind::AccountOrThread) => {
            let mut all: Vec<(String, String)> = if pools.dive {
                pools
                    .account_runtime
                    .as_ref()
                    .and_then(|r| pools.accounts.get(r))
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            all.extend(pools.threads.iter().cloned());
            all
        }
        Some(ArgKind::Free) | None => Vec::new(),
    };
    let rows = pool
        .into_iter()
        .map(|(value, detail)| {
            let row = Suggestion {
                label: value.clone(),
                value: value.clone(),
                detail,
                more: true,
            };
            (vec![value], row)
        })
        .collect();
    Suggestions {
        start,
        end: line.len(),
        command: false,
        mention: false,
        items: ranked(word, rows),
    }
}

/// The accounts `/new --account` offers, with their state, for the arguments `args` typed
/// so far: those of the runtime `--runtime` names (its runtime part: `claude-code` of
/// `claude-code:work`), else of the project's runtime as far as the TUI knows it
/// ([`Pools::project_runtimes`]), else of the default runtime ([`Pools::default_runtime`]);
/// with none known, every runtime's, each row's detail naming its runtime.
fn new_accounts(args: &[&str], pools: &Pools) -> Vec<(String, String)> {
    let (mut project, mut runtime): (Option<&str>, Option<&str>) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.split_once('=') {
            Some(("--runtime", v)) => runtime = Some(v),
            Some((f, _)) if f.starts_with("--") => {}
            _ if *a == "--runtime" => runtime = it.next().copied(),
            _ if *a == "--name" || *a == "--account" => {
                it.next();
            }
            _ if a.starts_with("--") => {}
            _ if project.is_none() => project = Some(*a),
            _ => {}
        }
    }
    let runtime = runtime
        .map(|r| twr_core::account::split_runtime(r).0)
        .or_else(|| {
            project
                .and_then(|p| pools.project_runtimes.get(p))
                .map(String::as_str)
        })
        .or(pools.default_runtime.as_deref());
    match runtime {
        Some(r) => pools.accounts.get(r).cloned().unwrap_or_default(),
        None => pools
            .accounts
            .iter()
            .flat_map(|(rt, names)| {
                names.iter().map(move |(name, state)| {
                    let detail = match state.as_str() {
                        "" => rt.clone(),
                        s => format!("{rt} · {s}"),
                    };
                    (name.clone(), detail)
                })
            })
            .collect(),
    }
}

/// Whether a whole `-` or `_` part of `key` starts with `word` (`webapp` in `orbit-webapp-1`).
fn part_starts_with(key: &str, word: &str) -> bool {
    key.split(['-', '_']).any(|p| p.starts_with(word))
}

/// The `@` mention rows for `line` with the cursor at byte `cursor`, when the word the cursor is
/// in or ends starts with `@` (the `@` at the start of the line or after whitespace): the
/// threads of [`Pools::mentions`] whose name starts with what follows the `@` up to the cursor,
/// then those with a `-`/`_` part of the name that starts with it (`@web` finds
/// `orbit-webapp-1`), then those with an alias that does either, case aside; live threads
/// before stopped ones, each group in the given order. A bare `@` lists them all. Only what is
/// before the cursor is matched, but each row puts in `@` and the thread's exact name in place
/// of the whole word: the `@` and what follows it up to the next whitespace (`ask @we|b`
/// completes to `ask @web`, not `ask @web b`). Empty for a `/` command line (whose popup is
/// [`suggest`]'s, an argument starting with `@` included) and when nothing matches.
pub fn mention(line: &str, cursor: usize, pools: &Pools) -> Suggestions {
    if line.starts_with('/') && !line.starts_with("//") {
        return Suggestions::default();
    }
    let Some(before) = line.get(..cursor) else {
        return Suggestions::default();
    };
    let start = after_last_space(before).unwrap_or(0);
    let Some(word) = before[start..].strip_prefix('@') else {
        return Suggestions::default();
    };
    let word = word.to_lowercase();
    // The rest of the word after the cursor goes too.
    let end = line[cursor..]
        .find(char::is_whitespace)
        .map_or(line.len(), |i| cursor + i);
    let mut rows: Vec<((bool, u8), Suggestion)> = pools
        .mentions
        .iter()
        .filter_map(|m| {
            let name = m.name.to_lowercase();
            let alias = || {
                m.aliases.iter().any(|a| {
                    let a = a.to_lowercase();
                    a.starts_with(&word) || part_starts_with(&a, &word)
                })
            };
            let tier = if name.starts_with(&word) {
                0
            } else if part_starts_with(&name, &word) {
                1
            } else if alias() {
                2
            } else {
                return None;
            };
            let value = format!("@{}", m.name);
            let row = Suggestion {
                label: value.clone(),
                value,
                detail: m.detail.clone(),
                more: true,
            };
            Some(((!m.live, tier), row))
        })
        .collect();
    // Stable: the given order holds within a group.
    rows.sort_by_key(|(rank, _)| *rank);
    Suggestions {
        start,
        end,
        command: false,
        mention: true,
        items: rows.into_iter().map(|(_, row)| row).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse("/summaries"), Ok(Slash::Summaries(None)));
        assert_eq!(parse("/summaries off"), Ok(Slash::Summaries(Some(false))));
        assert_eq!(parse("/summaries on"), Ok(Slash::Summaries(Some(true))));
        assert!(parse("/summaries soon").is_err());
        assert_eq!(
            parse("/orch codex o5"),
            Ok(Slash::Orch {
                runtime: "codex".into(),
                model: Some("o5".into())
            })
        );
        assert_eq!(
            parse("/orchestrator  grok-build "),
            Ok(Slash::Orch {
                runtime: "grok-build".into(),
                model: None
            })
        );
        assert_eq!(
            parse("/new ~/src/api --name api2 --runtime codex"),
            Ok(Slash::New {
                project: "~/src/api".into(),
                runtime: Some("codex".into()),
                name: Some("api2".into()),
                account: None,
                outside_pin: false,
            })
        );
        assert_eq!(
            parse("/new 'my project'"),
            Ok(Slash::New {
                project: "my project".into(),
                runtime: None,
                name: None,
                account: None,
                outside_pin: false,
            })
        );
        assert_eq!(parse("/park"), Ok(Slash::Park(None)));
        assert_eq!(parse("/open web"), Ok(Slash::Open(Some("web".into()))));
        assert_eq!(parse("/open"), Ok(Slash::Open(None)));
        assert_eq!(parse("/back"), Ok(Slash::Back));
        assert_eq!(parse("/unsend"), Ok(Slash::Unsend));
        assert!(parse("/unsend now").is_err());
        assert_eq!(
            parse("/mode auto"),
            Ok(Slash::Mode {
                thread: None,
                mode: "auto".into()
            })
        );
        assert_eq!(
            parse("/mode web plan"),
            Ok(Slash::Mode {
                thread: Some("web".into()),
                mode: "plan".into()
            })
        );
        assert_eq!(
            parse("/rename notifications-design"),
            Ok(Slash::Rename {
                thread: None,
                name: "notifications-design".into()
            })
        );
        assert_eq!(
            parse("/rename twr-1 notifications-design"),
            Ok(Slash::Rename {
                thread: Some("twr-1".into()),
                name: "notifications-design".into()
            })
        );
        assert_eq!(
            parse("/rename"),
            Err("usage: /rename [thread] <name>".into())
        );
        // `/model`: the thread first, as `twr model`; `--effort` and `--runtime` anywhere, `=`
        // or not.
        let model = |thread: Option<&str>, model: Option<&str>, effort: Option<&str>| {
            Ok(Slash::Model {
                thread: thread.map(str::to_string),
                model: model.map(str::to_string),
                effort: effort.map(str::to_string),
                runtime: None,
            })
        };
        assert_eq!(parse("/model"), model(None, None, None));
        assert_eq!(parse("/model api"), model(Some("api"), None, None));
        assert_eq!(
            parse("/model api claude-fable-5-1"),
            model(Some("api"), Some("claude-fable-5-1"), None)
        );
        assert_eq!(
            parse("/model api claude-fable-5-1 --effort high"),
            model(Some("api"), Some("claude-fable-5-1"), Some("high"))
        );
        assert_eq!(
            parse("/model api --effort high"),
            model(Some("api"), None, Some("high"))
        );
        assert_eq!(
            parse("/model --effort=low api fable"),
            model(Some("api"), Some("fable"), Some("low"))
        );
        assert_eq!(
            parse("/model --effort default"),
            model(None, None, Some("default"))
        );
        // A blank model is passed on: the hub reads it as the runtime's default.
        assert_eq!(parse("/model api ''"), model(Some("api"), Some(""), None));
        let moved = |thread: Option<&str>, m: Option<&str>, effort: Option<&str>, rt: &str| {
            Ok(Slash::Model {
                thread: thread.map(str::to_string),
                model: m.map(str::to_string),
                effort: effort.map(str::to_string),
                runtime: Some(rt.into()),
            })
        };
        assert_eq!(
            parse("/model api --runtime codex"),
            moved(Some("api"), None, None, "codex")
        );
        assert_eq!(
            parse("/model api gpt-6.1-sol --runtime=codex --effort xhigh"),
            moved(Some("api"), Some("gpt-6.1-sol"), Some("xhigh"), "codex")
        );
        assert_eq!(
            parse("/model --runtime 'acp:my agent'"),
            moved(None, None, None, "acp:my agent")
        );
        assert_eq!(parse("/delete api"), Ok(Slash::Delete(vec!["api".into()])));
        assert_eq!(
            parse("/delete api web"),
            Ok(Slash::Delete(vec!["api".into(), "web".into()]))
        );
        let trash = |t: TrashCmd| Ok(Slash::Trash(t));
        assert_eq!(parse("/trash"), trash(TrashCmd::List));
        assert_eq!(parse("/trash ls"), trash(TrashCmd::List));
        assert_eq!(
            parse("/trash empty abcd api"),
            trash(TrashCmd::Empty {
                items: vec!["abcd".into(), "api".into()],
                all: false
            })
        );
        assert_eq!(
            parse("/trash empty --all"),
            trash(TrashCmd::Empty {
                items: Vec::new(),
                all: true
            })
        );
        assert_eq!(
            parse("/trash restore abcd as api-2"),
            trash(TrashCmd::Restore {
                item: "abcd".into(),
                as_name: Some("api-2".into())
            })
        );
        assert_eq!(
            parse("/trash restore abcd"),
            trash(TrashCmd::Restore {
                item: "abcd".into(),
                as_name: None
            })
        );
        for bad in [
            "/trash empty",
            "/trash empty x --all",
            "/trash restore",
            "/trash restore a b",
            "/trash frob",
        ] {
            assert!(
                parse(bad).unwrap_err().starts_with("usage: /trash"),
                "{bad}"
            );
        }
        assert_eq!(parse("/stop api"), Ok(Slash::Stop(Some("api".into()))));
        assert_eq!(parse("/resume web"), Ok(Slash::Resume(Some("web".into()))));
        assert_eq!(parse("/trim web"), Ok(Slash::Trim(Some("web".into()))));
        assert_eq!(parse("/trim"), Ok(Slash::Trim(None)));
        // The context's compaction is no layout: `/compact` stays the phone layout's.
        assert_eq!(
            parse("/compact-context web"),
            Ok(Slash::CompactContext(Some("web".into())))
        );
        assert_eq!(parse("/compact-context"), Ok(Slash::CompactContext(None)));
        assert_eq!(parse("/compact"), Ok(Slash::Compact(None)));
        assert!(parse("/compact-context a b").is_err());
        assert_eq!(parse("/attach"), Ok(Slash::Attach(None)));
        assert_eq!(parse("/help"), Ok(Slash::Help));
        assert_eq!(parse("/parked"), Ok(Slash::Parked(None)));
        assert_eq!(parse("/parked on"), Ok(Slash::Parked(Some(true))));
        assert_eq!(parse("/parked off"), Ok(Slash::Parked(Some(false))));
        assert_eq!(parse("/sidebar"), Ok(Slash::Sidebar(None)));
        assert_eq!(parse("/spaces"), Ok(Slash::Spaces(None)));
        assert_eq!(parse("/spaces off"), Ok(Slash::Spaces(Some(false))));
        assert_eq!(parse("/settings"), Ok(Slash::Settings));
        assert!(parse("/settings x").is_err());
        assert_eq!(parse("/ws new"), Ok(Slash::WsNew(None)));
        assert_eq!(parse("/ws new api"), Ok(Slash::WsNew(Some("api".into()))));
        assert_eq!(parse("/ws api"), Ok(Slash::Ws(Some("api".into()))));
        assert_eq!(
            parse("/w"),
            Ok(Slash::Pane(crate::app::CompactPane::Workspaces))
        );
        assert_eq!(parse("/sidebar on"), Ok(Slash::Sidebar(Some(true))));
        assert_eq!(parse("/sidebar off"), Ok(Slash::Sidebar(Some(false))));
        assert_eq!(parse("/focus"), Ok(Slash::Focus(None)));
        assert_eq!(parse("/focus on"), Ok(Slash::Focus(Some(true))));
        assert_eq!(parse("/focus off"), Ok(Slash::Focus(Some(false))));
        assert_eq!(parse("/mouse"), Ok(Slash::Mouse(None)));
        assert_eq!(parse("/mouse off"), Ok(Slash::Mouse(Some(false))));
        assert_eq!(parse("/mouse on"), Ok(Slash::Mouse(Some(true))));
        // `/orch` alone picks the runtime; the views, pins and quitting.
        assert_eq!(parse("/orch"), Ok(Slash::OrchPick));
        assert_eq!(parse("/orchestrator"), Ok(Slash::OrchPick));
        assert_eq!(parse("/grid"), Ok(Slash::Grid));
        assert_eq!(parse("/decisions"), Ok(Slash::Decisions));
        assert_eq!(parse("/pin"), Ok(Slash::Pin(None)));
        assert_eq!(parse("/pin web"), Ok(Slash::Pin(Some("web".into()))));
        assert_eq!(parse("/quit"), Ok(Slash::Quit));
        for (bad, says) in [
            ("/frob", "unknown command /frob"),
            ("/orch a b c", "usage: /orch"),
            ("/grid now", "usage: /grid"),
            ("/decisions all", "usage: /decisions"),
            ("/pin a b", "usage: /pin [thread]"),
            ("/quit now", "usage: /quit"),
            ("/new", "usage: /new"),
            ("/new a b", "usage: /new"),
            ("/new a --name", "usage: /new"),
            ("/new a --name x --name y", "usage: /new"),
            ("/new a --box b", "unknown option --box"),
            ("/park a b", "usage: /park"),
            ("/help me", "usage: /help"),
            ("/mouse maybe", "usage: /mouse [on|off]"),
            ("/back now", "usage: /back"),
            ("/mode", "usage: /mode [thread] <mode>"),
            ("/mode a b c", "usage: /mode [thread] <mode>"),
            (
                "/model a b c",
                "usage: /model <thread> [model] [--effort e] [--runtime r]",
            ),
            ("/model a --effort", "usage: /model"),
            ("/model a --runtime", "usage: /model"),
            ("/model a --runtime x --runtime=y", "usage: /model"),
            ("/model a --effort x --effort y", "usage: /model"),
            ("/model a --effort=x --effort y", "usage: /model"),
            ("/model a --fast", "unknown option --fast; usage: /model"),
            ("/model a --fast=1", "unknown option --fast; usage: /model"),
            ("/delete", "usage: /delete <thread>..."),
            ("/open a b", "usage: /open"),
            ("/mouse on off", "usage: /mouse [on|off]"),
            ("/parked all", "usage: /parked [on|off]"),
            ("/sidebar all", "usage: /sidebar [on|off]"),
            ("/focus all", "usage: /focus [on|off]"),
            ("/focus on extra", "usage: /focus [on|off]"),
            ("/new 'a", "unclosed quote"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    #[test]
    fn tab_completes_commands_runtimes_and_threads() {
        let rts = vec![
            "claude-code".to_string(),
            "codex".into(),
            "grok-build".into(),
        ];
        let threads = vec!["api".to_string(), "api-v2".into(), "web".into()];
        let c = |s: &str| complete(s, &rts, &threads, &[]);
        assert_eq!(
            c("/pa"),
            (Some("/park".into()), vec!["/park".into(), "/parked".into()])
        );
        assert_eq!(c("/parke"), (Some("/parked ".into()), vec![]));
        assert_eq!(c("/he"), (Some("/help ".into()), vec![]));
        // Several: the common prefix, and the candidates.
        assert_eq!(
            c("/orc"),
            (
                Some("/orch".into()),
                vec!["/orch".to_string(), "/orchestrator".into()]
            )
        );
        assert_eq!(c("/mode w"), (Some("/mode web ".into()), vec![]));
        assert_eq!(c("/model w"), (Some("/model web ".into()), vec![]));
        assert_eq!(c("/mode").1, ["/mode", "/model"]);
        // Only the thread completes: a model or an effort is the runtime's own, and a
        // runtime may be any agent command or preset.
        assert_eq!(c("/model web c"), (None, vec![]));
        assert_eq!(c("/model web --runtime c"), (None, vec![]));
        assert_eq!(c("/del"), (Some("/delete ".into()), vec![]));
        assert_eq!(c("/delete w"), (Some("/delete web ".into()), vec![]));
        // Every thread of `/delete` completes.
        assert_eq!(
            c("/delete web w"),
            (Some("/delete web web ".into()), vec![])
        );
        assert_eq!(c("/mouse o").1, ["on", "off"]);
        assert_eq!(c("/parked of"), (Some("/parked off ".into()), vec![]));
        assert_eq!(c("/orch c").0, None);
        assert_eq!(c("/orch c").1, ["claude-code", "codex"]);
        assert_eq!(c("/orch cl"), (Some("/orch claude-code ".into()), vec![]));
        assert_eq!(
            c("/orchestrator g"),
            (Some("/orchestrator grok-build ".into()), vec![])
        );
        assert_eq!(c("/stop w"), (Some("/stop web ".into()), vec![]));
        assert_eq!(c("/park a").0, Some("/park api".into()));
        // A model, a project, an unknown command: nothing.
        assert_eq!(c("/orch codex o"), (None, vec![]));
        assert_eq!(c("/new a"), (None, vec![]));
        assert_eq!(c("/zz"), (None, vec![]));
    }

    #[test]
    fn tab_completion_splits_after_multibyte_whitespace() {
        let rts = vec!["codex".to_string()];
        let threads = vec!["api".to_string(), "web".into()];
        let c = |s: &str| complete(s, &rts, &threads, &[]);
        // U+2003 (3 bytes) and U+00A0 (2 bytes) end the command word like a space does.
        assert_eq!(
            c("/park\u{2003}a"),
            (Some("/park\u{2003}api ".into()), vec![])
        );
        assert_eq!(c("/stop\u{a0}w"), (Some("/stop\u{a0}web ".into()), vec![]));
        assert_eq!(
            c("/orch\u{2003}"),
            (Some("/orch\u{2003}codex ".into()), vec![])
        );
        assert_eq!(c("\u{2003}"), (None, vec![]));
        assert_eq!(c("/pa\u{a0}\u{2003}x"), (None, vec![]));
    }

    /// The popup: commands that start with the word first, then those that contain it, with
    /// their usage and description; then the values of the argument under way.
    #[test]
    fn suggestions_rank_prefix_matches_first() {
        let pools = Pools {
            runtimes: vec![
                ("claude-code".into(), "installed".into()),
                ("codex".into(), "not installed".into()),
            ],
            threads: vec![
                ("api".into(), "running".into()),
                ("web-api".into(), "idle".into()),
                ("docs".into(), "done".into()),
            ],
            dive: false,
            mentions: Vec::new(),
            workspaces: vec![
                ("default".into(), "current".into()),
                ("work".into(), "".into()),
            ],
            ..Default::default()
        };
        let names = |line: &str, pools: &Pools| -> Vec<String> {
            suggest(line, pools)
                .items
                .into_iter()
                .map(|i| i.value)
                .collect()
        };
        // Everything for a bare `/`, in the help's order.
        let all = suggest("/", &pools);
        assert!(all.command && all.start == 0);
        assert_eq!(all.items.len(), COMMANDS.len());
        assert_eq!(all.items[0].label, "/open [thread]");
        assert!(all.items[0].detail.contains("talk to a worker directly"));
        // Prefix first, then substring; an alias matches its command.
        assert_eq!(
            names("/o", &pools),
            [
                "/open",
                "/orch",
                "/mode",
                "/model",
                "/account",
                "/accounts",
                "/compact-context",
                "/stop",
                "/compact",
                "/focus",
                "/decisions",
                "/mouse",
                "/ws"
            ]
        );
        assert_eq!(names("/orchestr", &pools), ["/orch"]);
        assert_eq!(names("/se", &pools), ["/settings", "/unsend", "/mouse"]);
        assert_eq!(
            names("/P", &pools),
            [
                "/park",
                "/pin",
                "/parked",
                "/open",
                "/compact-context",
                "/stop",
                "/spaces",
                "/compact",
                "/ws",
                "/help"
            ]
        );
        // Taking arguments or not.
        let help = &suggest("/he", &pools).items[0];
        assert_eq!((help.value.as_str(), help.more), ("/help", false));
        assert!(suggest("/pa", &pools).items[0].more);
        // Arguments: threads, runtimes, modes, on/off.
        let s = suggest("/open api", &pools);
        assert_eq!((s.start, s.command), (6, false));
        assert_eq!(names("/open api", &pools), ["api", "web-api"]);
        assert_eq!(names("/stop d", &pools), ["docs"]);
        assert_eq!(suggest("/orch c", &pools).items[1].detail, "not installed");
        assert_eq!(names("/mode ", &pools), ["api", "web-api", "docs"]);
        assert_eq!(
            names("/mode web-api a", &pools),
            [
                "acceptEdits",
                "auto",
                "manual",
                "default",
                "plan",
                "read-only",
                "dontAsk",
                "bypassPermissions"
            ]
        );
        let dive = Pools {
            dive: true,
            ..pools.clone()
        };
        assert_eq!(names("/mode au", &dive), ["auto", "default"]);
        assert_eq!(names("/mode do", &dive), ["dontAsk", "docs"]);
        assert_eq!(names("/mouse ", &pools), ["on", "off"]);
        assert_eq!(names("/model ", &dive), ["api", "web-api", "docs"]);
        assert!(names("/model api ", &pools).is_empty());
        assert_eq!(names("/delete ", &pools), ["api", "web-api", "docs"]);
        assert_eq!(names("/delete api ", &pools), ["api", "web-api", "docs"]);
        // Nothing: not a command, a message line, no arguments, an unknown command.
        for line in [
            "hello",
            "//tmp",
            "/help ",
            "/new ~/src",
            "/zz x",
            "/open a b",
        ] {
            assert!(suggest(line, &pools).items.is_empty(), "{line}");
        }
    }

    /// Threads for the mention tests, in this order: `orbit-webapp-1` (live), `webhooks`
    /// (stopped), `web` (live, alias `site`), `api_web` (live), `docs` (stopped, alias
    /// `web-docs`).
    fn mention_pools() -> Pools {
        let m = |name: &str, aliases: &[&str], live: bool| Mentionable {
            name: name.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            detail: if live {
                "running · fake"
            } else {
                "done · fake"
            }
            .into(),
            live,
        };
        Pools {
            mentions: vec![
                m("orbit-webapp-1", &[], true),
                m("webhooks", &[], false),
                m("web", &["site"], true),
                m("api_web", &[], true),
                m("docs", &["web-docs"], false),
            ],
            ..Pools::default()
        }
    }

    /// The values of the mention rows for `line` with the cursor at its end.
    fn mentions(line: &str, pools: &Pools) -> Vec<String> {
        mention(line, line.len(), pools)
            .items
            .into_iter()
            .map(|i| i.value)
            .collect()
    }

    /// Live before stopped; within each, a name that starts with the word, then a name with a
    /// `-`/`_` part that does, then an alias; case aside. A bare `@` lists every thread.
    #[test]
    fn mentions_rank_prefix_then_name_part_then_alias() {
        let pools = mention_pools();
        assert_eq!(
            mentions("@web", &pools),
            ["@web", "@orbit-webapp-1", "@api_web", "@webhooks", "@docs"]
        );
        assert_eq!(mentions("@WeB", &pools), mentions("@web", &pools));
        assert_eq!(
            mentions("@", &pools),
            ["@orbit-webapp-1", "@web", "@api_web", "@webhooks", "@docs"]
        );
        assert_eq!(mentions("@orb", &pools), ["@orbit-webapp-1"]);
        assert_eq!(mentions("@orbit-w", &pools), ["@orbit-webapp-1"]);
        assert_eq!(mentions("@1", &pools), ["@orbit-webapp-1"]);
        // An alias finds the thread, and its exact name goes in.
        assert_eq!(mentions("@SI", &pools), ["@web"]);
        assert_eq!(mentions("@doc", &pools), ["@docs"]);
        // A part only matches from its start, a name only as a prefix.
        assert!(mentions("@ebapp", &pools).is_empty());
        assert!(mentions("@zz", &pools).is_empty());
        // The row: the name with its `@`, the detail, a space after it.
        let s = mention("hi @api", 7, &pools);
        assert!(s.mention && !s.command);
        assert_eq!((s.start, s.end), (3, 7));
        let row = &s.items[0];
        assert_eq!(
            (row.label.as_str(), row.detail.as_str(), row.more),
            ("@api_web", "running · fake", true)
        );
    }

    /// The `@` must start a word: at the start of the line or after any whitespace, multibyte
    /// included. A `/` command line has no mentions; a `//` line is a message and has them.
    #[test]
    fn a_mention_starts_a_word() {
        let pools = mention_pools();
        let s = mention("ask\u{2003}@we", "ask\u{2003}@we".len(), &pools);
        assert_eq!((s.start, s.end), (6, 9));
        assert_eq!(s.items[0].value, "@web");
        assert_eq!(mentions("x\u{a0}@", &pools).len(), 5);
        assert_eq!(mentions("@we", &pools)[0], "@web");
        for line in ["mail@we", "ask @we ", "hello", "", "/open @we", "/stop @"] {
            assert!(mentions(line, &pools).is_empty(), "{line:?}");
        }
        assert_eq!(mentions("//tmp is full, @si", &pools), ["@web"]);
    }

    /// The word is the one the cursor is in or ends, wherever it is in the line: matched on
    /// what is before the cursor, replaced through its end.
    #[test]
    fn a_mention_is_the_word_before_the_cursor() {
        let pools = mention_pools();
        let line = "tell @we and @do now";
        let at = |word: &str| line.find(word).unwrap() + word.len();
        let s = mention(line, at("@we"), &pools);
        assert_eq!((s.start, s.end), (5, 8));
        assert_eq!(s.items[0].value, "@web");
        let s = mention(line, at("@do"), &pools);
        assert_eq!((s.start, s.end), (13, 16));
        let names: Vec<&str> = s.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(names, ["@docs"]);
        // Inside a word, the part before the cursor (`@w` of `@we`) is matched, and the
        // whole word is replaced, up to any whitespace (multibyte included).
        let s = mention(line, 7, &pools);
        assert_eq!(s.items.len(), 5);
        assert_eq!((s.start, s.end), (5, 8));
        let s = mention("ask @we\u{e9}b\u{2003}x", 7, &pools);
        assert_eq!((s.start, s.end), (4, "ask @we\u{e9}b".len()));
        assert_eq!(s.items[0].value, "@web");
        let s = mention("@weXYZ", 1, &pools);
        assert_eq!((s.start, s.end, s.items.len()), (0, 6, 5));
        // Not after a mention's word, nor on a char boundary that is not one.
        assert!(mention(line, line.len(), &pools).items.is_empty());
        assert!(mention(line, at("and"), &pools).items.is_empty());
        assert!(mention("@é", 2, &pools).items.is_empty());
    }

    #[test]
    fn words_split_on_whitespace_and_quotes() {
        assert_eq!(words(r#"a "b c" 'd' e"#).unwrap(), ["a", "b c", "d", "e"]);
        assert_eq!(words("x ''").unwrap(), ["x", ""]);
    }

    /// `/account [thread] [name] [--outside-pin]`, `/account add <runtime> <name>`,
    /// `/accounts`, and `/new --account`.
    #[test]
    fn account_commands_parse() {
        let account = |thread: Option<&str>, account: Option<&str>, outside_pin| {
            Ok(Slash::Account {
                thread: thread.map(str::to_string),
                account: account.map(str::to_string),
                outside_pin,
            })
        };
        assert_eq!(parse("/account"), account(None, None, false));
        assert_eq!(parse("/account api"), account(Some("api"), None, false));
        assert_eq!(
            parse("/account api work"),
            account(Some("api"), Some("work"), false)
        );
        assert_eq!(
            parse("/account --outside-pin api work"),
            account(Some("api"), Some("work"), true)
        );
        // In a dive one word may be the account: the app decides.
        assert_eq!(
            parse("/account work --outside-pin"),
            account(Some("work"), None, true)
        );
        assert!(parse("/account --outside-pin").is_err());
        assert!(parse("/account a b c").is_err());
        assert_eq!(
            parse("/account --force api"),
            Err(
                "unknown option --force; usage: /account [thread] [name] [--outside-pin] | add \
                 <runtime> <name>"
                    .into()
            )
        );
        assert_eq!(
            parse("/account add claude work"),
            Ok(Slash::AccountAdd {
                runtime: "claude".into(),
                name: "work".into()
            })
        );
        // `add` alone or with one word is a thread's name.
        assert_eq!(parse("/account add"), account(Some("add"), None, false));
        assert_eq!(
            parse("/account add work"),
            account(Some("add"), Some("work"), false)
        );
        assert_eq!(parse("/accounts"), Ok(Slash::Accounts));
        assert!(parse("/accounts claude").is_err());
        assert_eq!(
            parse("/new api --account work --outside-pin"),
            Ok(Slash::New {
                project: "api".into(),
                runtime: None,
                name: None,
                account: Some("work".into()),
                outside_pin: true,
            })
        );
        assert_eq!(
            parse("/new api --runtime claude-code:work"),
            Ok(Slash::New {
                project: "api".into(),
                runtime: Some("claude-code:work".into()),
                name: None,
                account: None,
                outside_pin: false,
            })
        );
        assert_eq!(
            parse("/new api --runtime claude-code:work --outside-pin"),
            Ok(Slash::New {
                project: "api".into(),
                runtime: Some("claude-code:work".into()),
                name: None,
                account: None,
                outside_pin: true,
            })
        );
        assert!(parse("/new api --outside-pin").is_err());
        assert!(parse("/new api --runtime claude-code --outside-pin").is_err());
        assert!(parse("/new api --account").is_err());
        // `--option=value` as `--option value`.
        assert_eq!(
            parse("/new api --account=work --runtime=claude-code --name=api2 --outside-pin"),
            Ok(Slash::New {
                project: "api".into(),
                runtime: Some("claude-code".into()),
                name: Some("api2".into()),
                account: Some("work".into()),
                outside_pin: true,
            })
        );
        assert!(parse("/new api --account=work --account x").is_err());
        assert!(parse("/new api --outside-pin=yes --account work").is_err());
        assert!(parse("/new api --box=b").is_err());
    }

    /// `/new <project> --account ` (and `--account=`) completes the accounts of the runtime
    /// the line names (`--runtime`, its runtime part), else of the project's runtime as the
    /// TUI knows it, else of the default runtime; with none known, every runtime's.
    #[test]
    fn new_completes_the_account_of_its_runtime() {
        let accounts = [
            (
                "claude-code".to_string(),
                vec![
                    ("personal".to_string(), "native, logged in".to_string()),
                    ("work".to_string(), "logged in".to_string()),
                ],
            ),
            (
                "codex".to_string(),
                vec![("default".to_string(), "native".to_string())],
            ),
        ];
        let mut pools = Pools {
            accounts: accounts.into_iter().collect(),
            project_runtimes: [("gen".to_string(), "codex".to_string())]
                .into_iter()
                .collect(),
            default_runtime: Some("claude-code".into()),
            ..Default::default()
        };
        let rows = |line: &str, pools: &Pools| -> Vec<(String, String)> {
            suggest(line, pools)
                .items
                .into_iter()
                .map(|i| (i.value, i.detail))
                .collect()
        };
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        let claude = [
            pair("personal", "native, logged in"),
            pair("work", "logged in"),
        ];
        // The default runtime.
        assert_eq!(rows("/new api --account ", &pools), claude);
        assert_eq!(
            rows("/new api --account w", &pools),
            [pair("work", "logged in")]
        );
        // The project's runtime.
        assert_eq!(
            rows("/new gen --account ", &pools),
            [pair("default", "native")]
        );
        // `--runtime`, before or after, with or without `=`, its account part aside.
        assert_eq!(
            rows("/new api --runtime codex --account ", &pools),
            [pair("default", "native")]
        );
        assert_eq!(
            rows(
                "/new gen --runtime=claude-code:work --name x --account ",
                &pools
            ),
            claude
        );
        // `--account=`: the rows replace what follows `=`.
        let s = suggest("/new gen --runtime claude-code --account=wo", &pools);
        assert_eq!(
            (s.start, s.items.len(), s.items[0].value.as_str()),
            ("/new gen --runtime claude-code --account=".len(), 1, "work")
        );
        let s = suggest("/new api --account=", &pools);
        assert_eq!(s.items.len(), 2);
        // Nothing else completes there.
        assert!(rows("/new api ", &pools).is_empty());
        assert!(rows("/new api --name ", &pools).is_empty());
        // A runtime with no accounts listed: none.
        assert!(rows("/new api --runtime gemini --account ", &pools).is_empty());
        // No runtime known: every runtime's, named.
        pools.default_runtime = None;
        assert_eq!(
            rows("/new api --account ", &pools),
            [
                pair("personal", "claude-code · native, logged in"),
                pair("work", "claude-code · logged in"),
                pair("default", "codex · native"),
            ]
        );
    }

    /// `/account` completes threads, then the accounts of the thread named (else of the
    /// thread dived into or selected), with their state; in a dive its first word is an
    /// account too. `/account add` completes the runtimes.
    #[test]
    fn account_arguments_complete_to_accounts() {
        let accounts = [
            (
                "claude-code".to_string(),
                vec![
                    ("personal".to_string(), "native, logged in".to_string()),
                    ("work".to_string(), "logged in".to_string()),
                ],
            ),
            (
                "codex".to_string(),
                vec![("default".to_string(), "native".to_string())],
            ),
        ];
        let mut pools = Pools {
            threads: vec![
                ("api".into(), "idle · claude-code:work · mode auto".into()),
                ("gen".into(), "idle · codex · mode auto".into()),
            ],
            accounts: accounts.into_iter().collect(),
            thread_runtimes: [
                ("api".to_string(), "claude-code".to_string()),
                ("gen".to_string(), "codex".to_string()),
            ]
            .into_iter()
            .collect(),
            account_runtime: Some("codex".into()),
            ..Default::default()
        };
        let rows = |line: &str, pools: &Pools| -> Vec<(String, String)> {
            suggest(line, pools)
                .items
                .into_iter()
                .map(|i| (i.value, i.detail))
                .collect()
        };
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            rows("/account ", &pools),
            [
                pair("api", "idle · claude-code:work · mode auto"),
                pair("gen", "idle · codex · mode auto")
            ]
        );
        assert_eq!(
            rows("/account api ", &pools),
            [
                pair("personal", "native, logged in"),
                pair("work", "logged in")
            ]
        );
        assert_eq!(rows("/account api w", &pools), [pair("work", "logged in")]);
        // No thread named: the selected one's runtime.
        assert_eq!(rows("/account nope ", &pools), [pair("default", "native")]);
        pools.dive = true;
        pools.account_runtime = Some("claude-code".into());
        let first: Vec<String> = rows("/account ", &pools).into_iter().map(|r| r.0).collect();
        assert_eq!(first, ["personal", "work", "api", "gen"]);
        assert_eq!(
            rows("/account add ", &pools),
            [pair("claude-code", "runtime"), pair("codex", "runtime")]
        );
        assert!(rows("/account add codex ", &pools).is_empty());
    }
}

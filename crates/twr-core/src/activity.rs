//! What a thread is doing right now, in a few words: the phrases of its activity line (the
//! hub's `thread.activity` and the thread object's `activity`, on every card and tile). They
//! are pure functions of what the log says, so the hub's live view and a replay of the log
//! read the same, and they cost no model call.
//!
//! A tool call says what it does ([`tool_phrase`]: `running tests`, `editing ui.rs`,
//! `researching codex login flow`), from the purpose the agent gave it when it gave one
//! (`ext.twr.purpose`, [`crate::event::PURPOSE_KEY`]: Claude Code's Bash `description`), else
//! from its tool and input. What the agent says it is about to do (`Let me check the scroll
//! tests.`) reads as an intent ([`intent`]: `checking the scroll tests`), and the step its own
//! plan has in progress (`ext.twr.step`, [`crate::event::STEP_KEY`]: Claude Code's todo list)
//! comes before either.

use crate::reduce::{ThreadStatus, ThreadView};

/// Longest activity line, in characters.
pub const ACTIVITY_MAX: usize = 160;

/// Longest phrase taken from what the agent wrote or gave (an intent, a purpose, a step), in
/// characters.
pub const PHRASE_MAX: usize = 80;

/// What `v` is doing, from its status alone: what a client shows before the hub said more.
pub fn fallback(v: &ThreadView) -> &'static str {
    match v.status() {
        ThreadStatus::Attached => "attached: driven in a terminal",
        ThreadStatus::Parked => "parked",
        ThreadStatus::Waiting => "waiting for you",
        ThreadStatus::Background => "waiting on background tasks",
        ThreadStatus::Running => "working",
        ThreadStatus::Error if v.crashed => "crashed",
        ThreadStatus::Error => "failed",
        ThreadStatus::Done if v.stopped => "stopped",
        ThreadStatus::Done => "done",
        ThreadStatus::Idle if v.turns == 0 => "not started",
        ThreadStatus::Idle => "idle",
    }
}

/// `s` on one line, cut to `max` characters at a word when it can be, with `…`.
pub fn cut(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    // At the last space, unless that leaves less than half.
    if let Some(i) = out.rfind(' ').filter(|&i| i >= out.len() / 2) {
        out.truncate(i);
    }
    out.push('…');
    out
}

/// What a call of `tool` with input summary `summary` does, in a few words. `purpose` is what
/// the agent said the call is for, when it said: an imperative (`Run the TUI tests`), read
/// as what it does (`running the TUI tests`).
pub fn tool_phrase(tool: &str, summary: &str, purpose: Option<&str>) -> String {
    if let Some(p) = purpose.map(str::trim).filter(|p| !p.is_empty()) {
        return cut(&gerund(p), PHRASE_MAX);
    }
    let s = summary.trim();
    let phrase = match tool {
        "Bash" | "shell" | "execute" => command_phrase(s),
        "BashOutput" | "TaskOutput" => "checking a background command".into(),
        "KillShell" | "KillBash" => "stopping a background command".into(),
        "Read" | "read" => on("reading", &files(s), "files"),
        "Write" => on("writing", &files(s), "a file"),
        "Edit" | "MultiEdit" | "edit" | "apply_patch" | "NotebookEdit" => {
            on("editing", &files(s), "files")
        }
        "delete" => on("deleting", &files(s), "files"),
        "move" => on("moving", &files(s), "files"),
        "Grep" | "Glob" | "search" => {
            let pattern = s.split(" in ").next().unwrap_or_default().trim();
            on("searching for", pattern, "the code")
        }
        "WebSearch" | "web_search" => on("researching", s, "the web"),
        "WebFetch" | "fetch" => on("reading", &host(s), "a web page"),
        "Task" | "Agent" => on("delegating:", s, "a subagent's task"),
        "TodoWrite" | "todo_write" | "update_plan" | "TaskCreate" | "TaskUpdate" | "TaskList"
        | "TaskGet" => "planning".into(),
        "ExitPlanMode" | "switch_mode" => "presenting a plan".into(),
        "AskUserQuestion" => "asking you a question".into(),
        "think" => "thinking".into(),
        _ => match tool.strip_prefix("mcp__") {
            Some(rest) => match rest.split_once("__") {
                Some((server, name)) => format!("using {server} {name}"),
                None => format!("using {rest}"),
            },
            None if tool.is_empty() || tool == "other" => "using a tool".into(),
            None => format!("using {tool}"),
        },
    };
    cut(&phrase, PHRASE_MAX)
}

/// `verb` and what it is about, or `verb` and `none` without that.
fn on(verb: &str, what: &str, none: &str) -> String {
    let what = what.trim();
    if what.is_empty() {
        format!("{verb} {none}")
    } else {
        format!("{verb} {what}")
    }
}

/// The file names in a tool's input summary, without their directories: a path (`/a/b/ui.rs`),
/// a list of them (`a.rs, b.rs`), or a title that ends with one (`Read src/ui.rs`); Codex's
/// `edit 5 files: a.rs, b.rs, c.rs, d.rs and 1 more` keeps its count of the others.
fn files(summary: &str) -> String {
    let more = summary.rsplit_once(" and ").filter(|(_, m)| {
        m.strip_suffix(" more")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    });
    let list = more.map_or(summary, |(list, _)| list);
    let names = list
        .split(", ")
        .filter_map(|part| part.split_whitespace().last())
        .map(|p| p.trim_matches(|c| matches!(c, '`' | '"' | '\'' | '(' | ')')))
        .map(|p| p.trim_end_matches('/').rsplit('/').next().unwrap_or(p))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    match more {
        Some((_, more)) => format!("{names} and {more}"),
        None => names,
    }
}

/// The host of `url` (`docs.rs` of `https://docs.rs/x`), or `url` when it has none.
fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

/// How much a command's phrase says about the work: a chain of commands is named by the one
/// that says the most (`cargo fmt && cargo test`: running tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Weight {
    Other,
    Format,
    Build,
    Lint,
    Test,
}

/// What shell command `cmd` does, in a few words: `running tests`, `committing`, `reading
/// app.rs`, else `running <program>`.
pub fn command_phrase(cmd: &str) -> String {
    weighed(cmd).map_or_else(|| "running a command".into(), |(_, p)| p)
}

/// [`command_phrase`] with its [`Weight`], when a command of `cmd` says something.
fn weighed(cmd: &str) -> Option<(Weight, String)> {
    let mut best: Option<(Weight, String)> = None;
    for segment in segments(cmd) {
        let words = words(&segment);
        let Some((weight, phrase)) = program_phrase(&words) else {
            continue;
        };
        if best.as_ref().is_none_or(|(w, _)| weight > *w) {
            best = Some((weight, phrase));
        }
    }
    best
}

/// The commands of a command line that do something: what `&&`, `||`, `;`, `&` and line
/// breaks separate, each without the filters it pipes into (`| tail -20`). A separator inside
/// quotes or escaped with `\` separates nothing (`git commit -m 'fix; cargo test'` is one
/// command).
fn segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // In the filters of a pipeline: what comes until the next separator is left out.
    let mut piped = false;
    let mut quote = None;
    let mut chars = cmd.chars().peekable();
    let mut end = |cur: &mut String, piped: bool| {
        let s = cur.trim();
        if !piped && !s.is_empty() {
            out.push(s.to_string());
        }
        cur.clear();
    };
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                    continue;
                }
                cur.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '\\' => {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '|' if chars.peek() == Some(&'|') => {
                    chars.next();
                    end(&mut cur, piped);
                    piped = false;
                }
                '|' => {
                    end(&mut cur, piped);
                    piped = true;
                }
                // (Not a redirection's `&`: `2>&1`, `&>`.)
                '&' if cur.ends_with(['>', '<']) || chars.peek() == Some(&'>') => cur.push(c),
                '&' | ';' | '\n' => {
                    if c == '&' && chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    end(&mut cur, piped);
                    piped = false;
                }
                _ => cur.push(c),
            },
        }
    }
    end(&mut cur, piped);
    out
}

/// The words of `segment`, quotes and escapes removed (a quoted string is one word).
fn words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            // In double quotes, `\` escapes what would end or expand them.
            Some('"') if c == '\\' && matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                word.extend(chars.next());
            }
            Some(_) => word.push(c),
            // Unquoted, `\` escapes the next character, whatever it is.
            None if c == '\\' => {
                word.extend(chars.next());
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => word.push(c),
        }
    }
    if started || !word.is_empty() {
        out.push(word);
    }
    out
}

/// Programs that run another one, named after their options (`timeout 600 cargo test`).
const WRAPPERS: &[&str] = &[
    "sudo",
    "time",
    "nice",
    "nohup",
    "exec",
    "command",
    "env",
    "timeout",
    "systemd-run",
    "xargs",
    "stdbuf",
    "unbuffer",
    "caffeinate",
    "npx",
    "uvx",
    "bunx",
];

/// Options of [`WRAPPERS`] that take a value (`env -u NAME`, `nice -n 10`, `sudo -u root`).
const VALUED: &[&str] = &["-u", "-n", "-s", "-k", "-p", "-g", "-C", "-E", "-I", "-P"];

/// Programs that do nothing to name (`cd`, `echo`).
const QUIET: &[&str] = &[
    "cd", "pushd", "popd", "export", "source", ".", "set", "unset", "alias", "echo", "printf",
    "true", "false", ":", "exit", "wait",
];

/// The phrase and weight of one command, its `words`; None for one that does nothing to name.
fn program_phrase(words: &[String]) -> Option<(Weight, String)> {
    let mut rest = words;
    // Variables set for it (`RUST_LOG=debug cargo test`), and what runs it.
    loop {
        let first = rest.first()?;
        if is_assignment(first) {
            rest = &rest[1..];
        } else if WRAPPERS.contains(&base(first)) {
            rest = &rest[1..];
            // Its options and their values, its durations (`-u X`, `--wait`, `600`, `10m`).
            while let Some(w) = rest.first() {
                let numeric = w.chars().next().is_some_and(|c| c.is_ascii_digit());
                if VALUED.contains(&w.as_str()) {
                    rest = rest.get(2..).unwrap_or_default();
                } else if w.starts_with('-') || numeric || is_assignment(w) {
                    rest = &rest[1..];
                } else {
                    break;
                }
            }
        } else {
            break;
        }
    }
    let program = base(rest.first()?);
    if QUIET.contains(&program) {
        return None;
    }
    // Its arguments, without redirections and their files (`2>&1`, `> out.log`, `&> log`).
    let mut args: Vec<&str> = Vec::new();
    let mut it = rest[1..].iter();
    while let Some(a) = it.next() {
        let op = a.trim_start_matches(|c: char| c.is_ascii_digit());
        if op.starts_with(['<', '>']) || op.starts_with("&>") {
            if op.trim_start_matches(['<', '>', '&']).is_empty() {
                it.next();
            }
            continue;
        }
        args.push(a);
    }
    // The first argument that is no option.
    let sub = |skip: &[&str]| -> Option<&str> {
        let mut it = args.iter().copied();
        while let Some(a) = it.next() {
            if skip.contains(&a) {
                it.next();
            } else if !a.starts_with('-') && !a.starts_with('+') {
                return Some(a);
            }
        }
        None
    };
    let last_file = || {
        args.iter()
            .rev()
            .find(|a| !a.starts_with('-'))
            .map(|a| base(a).to_string())
    };
    use Weight::*;
    let named = |w: Weight, s: &str| Some((w, s.to_string()));
    match program {
        // A shell running a script: what the script does.
        "bash" | "sh" | "zsh" | "dash" | "fish" => {
            let script = args
                .iter()
                .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                .and_then(|i| args.get(i + 1));
            match script {
                Some(script) => weighed(script).or_else(|| named(Other, "running a script")),
                None => match sub(&[]) {
                    Some(file) => named(Other, &format!("running {}", base(file))),
                    None => named(Other, &format!("running {program}")),
                },
            }
        }
        "cargo" => match sub(&[]).unwrap_or_default() {
            "test" | "t" | "nextest" => named(Test, "running tests"),
            "bench" => named(Test, "running benchmarks"),
            "clippy" => named(Lint, "linting"),
            "fmt" => named(Format, "formatting"),
            "build" | "b" => named(Build, "building"),
            "check" | "c" => named(Build, "checking the build"),
            "doc" => named(Build, "building the docs"),
            "add" | "update" | "install" | "remove" => named(Other, "updating dependencies"),
            "run" | "r" => named(Other, "running the program"),
            "" => named(Other, "running cargo"),
            other => named(Other, &format!("running cargo {other}")),
        },
        "npm" | "pnpm" | "yarn" | "bun" | "deno" => {
            let s = match sub(&[]).unwrap_or_default() {
                "run" | "run-script" => args
                    .iter()
                    .skip_while(|a| !matches!(**a, "run" | "run-script"))
                    .nth(1)
                    .copied()
                    .unwrap_or_default(),
                other => other,
            };
            script_phrase(program, s)
        }
        "make" | "just" | "task" | "mise" => {
            script_phrase(program, sub(&["-C", "-f"]).unwrap_or("build"))
        }
        "go" => match sub(&[]).unwrap_or_default() {
            "test" => named(Test, "running tests"),
            "build" | "install" => named(Build, "building"),
            "vet" => named(Lint, "linting"),
            "fmt" => named(Format, "formatting"),
            "run" => named(Other, "running the program"),
            "get" | "mod" => named(Other, "updating dependencies"),
            other => named(Other, &format!("running go {other}")),
        },
        "pytest" | "py.test" | "jest" | "vitest" | "mocha" | "rspec" | "phpunit" | "tox"
        | "nox" | "ctest" | "playwright" | "cypress" => named(Test, "running tests"),
        "python" | "python3" | "uv" | "poetry" => {
            let module = args
                .iter()
                .skip_while(|a| **a != "-m")
                .nth(1)
                .copied()
                .or_else(|| {
                    sub(&["-c"])
                        .filter(|s| *s == "run")
                        .and(args.get(1).copied())
                });
            match module {
                Some("pytest" | "unittest") => named(Test, "running tests"),
                Some("mypy" | "ruff" | "flake8" | "pylint") => named(Lint, "linting"),
                Some("black") => named(Format, "formatting"),
                Some("pip") => named(Other, "updating dependencies"),
                _ => match sub(&["-c"]) {
                    Some(script) => named(Other, &format!("running {}", base(script))),
                    None => named(Other, &format!("running {program}")),
                },
            }
        }
        "pip" | "pip3" | "brew" | "apt" | "apt-get" => named(Other, "installing packages"),
        "eslint" | "ruff" | "flake8" | "pylint" | "mypy" | "golangci-lint" | "shellcheck"
        | "clippy-driver" | "biome" | "oxlint" => {
            if sub(&[]) == Some("format") {
                named(Format, "formatting")
            } else {
                named(Lint, "linting")
            }
        }
        "prettier" | "black" | "gofmt" | "rustfmt" | "isort" | "dprint" | "taplo" => {
            named(Format, "formatting")
        }
        "tsc" | "pyright" => named(Build, "checking types"),
        "git" => git_phrase(sub(&["-C", "-c"]).unwrap_or_default()),
        "gh" => match (sub(&[]).unwrap_or_default(), args.get(1).copied()) {
            ("pr", Some("create")) => named(Other, "opening a pull request"),
            ("pr", Some("merge")) => named(Other, "merging a pull request"),
            ("pr", _) => named(Other, "checking a pull request"),
            ("run" | "workflow", _) => named(Other, "checking CI"),
            ("issue", _) => named(Other, "reading issues"),
            ("api", _) => named(Other, "querying GitHub"),
            _ => named(Other, "using gh"),
        },
        "grep" | "rg" | "ag" | "ack" | "find" | "fd" | "locate" => {
            named(Other, "searching the code")
        }
        "cat" | "head" | "tail" | "less" | "more" | "bat" | "nl" | "wc" | "jq" | "sed" | "awk"
        | "diff" | "xxd" | "od" | "hexdump" => {
            let edits = program == "sed" && args.iter().any(|a| a.starts_with("-i"));
            let verb = if edits { "editing" } else { "reading" };
            match last_file() {
                Some(f) if !f.is_empty() => named(Other, &format!("{verb} {f}")),
                _ => named(Other, &format!("{verb} files")),
            }
        }
        "ls" | "tree" | "pwd" | "du" | "df" | "stat" | "file" | "which" | "whereis" | "type"
        | "realpath" | "readlink" => named(Other, "looking around"),
        "curl" | "wget" | "http" | "xh" => {
            let url = args
                .iter()
                .find(|a| a.contains("://"))
                .copied()
                .unwrap_or_default();
            named(Other, &on("fetching", &host(url), "a URL"))
        }
        "sleep" => named(Other, "waiting"),
        "kill" | "pkill" | "killall" => named(Other, "stopping a process"),
        "rm" | "mv" | "cp" | "mkdir" | "touch" | "chmod" | "chown" | "ln" | "rmdir" | "tar"
        | "unzip" | "zip" => named(Other, "managing files"),
        "docker" | "podman" => match sub(&[]).unwrap_or_default() {
            "build" => named(Build, "building an image"),
            "compose" | "run" | "up" | "start" | "exec" => named(Other, "running containers"),
            _ => named(Other, &format!("using {program}")),
        },
        "ps" | "top" | "htop" | "pgrep" | "lsof" | "ss" | "netstat" | "journalctl"
        | "systemctl" | "free" | "uptime" => named(Other, "checking the system"),
        "twr" => named(Other, "using twr"),
        _ => named(Other, &format!("running {program}")),
    }
}

/// `program`'s script or target `s` (`npm test`, `make lint`).
fn script_phrase(program: &str, s: &str) -> Option<(Weight, String)> {
    use Weight::*;
    let s = s.to_ascii_lowercase();
    let w = |w: Weight, p: &str| Some((w, p.to_string()));
    if s.starts_with("test") || s.ends_with("test") || s.starts_with("e2e") || s == "check-all" {
        return w(Test, "running tests");
    }
    match s.as_str() {
        "lint" | "clippy" | "check" | "vet" => w(Lint, "linting"),
        "fmt" | "format" | "prettier" => w(Format, "formatting"),
        "build" | "compile" | "all" | "dist" | "bundle" => w(Build, "building"),
        "typecheck" | "tsc" | "types" => w(Build, "checking types"),
        "install" | "i" | "add" | "ci" | "update" | "upgrade" => {
            w(Other, "installing dependencies")
        }
        "dev" | "start" | "serve" | "preview" => w(Other, "running the dev server"),
        "" => w(Other, &format!("running {program}")),
        other => w(Other, &format!("running {program} {other}")),
    }
}

/// What `git <sub>` does.
fn git_phrase(sub: &str) -> Option<(Weight, String)> {
    let phrase = match sub {
        "commit" => "committing",
        "push" => "pushing",
        "pull" | "fetch" => "fetching changes",
        "rebase" => "rebasing",
        "merge" | "cherry-pick" => "merging",
        "diff" | "status" | "show" => "reviewing changes",
        "log" | "blame" | "shortlog" | "reflog" => "reading the history",
        "checkout" | "switch" | "branch" => "switching branches",
        "add" | "rm" | "mv" | "restore" | "reset" => "staging changes",
        "stash" => "stashing changes",
        "worktree" => "managing worktrees",
        "clone" => "cloning a repository",
        "tag" => "tagging",
        "" => "using git",
        other => return Some((Weight::Other, format!("running git {other}"))),
    };
    Some((Weight::Other, phrase.to_string()))
}

/// `word` sets a variable (`NAME=value`).
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with('-')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A path's last component (`cargo` of `~/.cargo/bin/cargo`).
fn base(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Openings of a sentence that says what the agent is about to do (`Let me check X.`, `Now
/// I'll run the tests.`), lowercase; what follows them is the intent.
const INTENT_OPENINGS: &[&str] = &[
    "let me ",
    "let's ",
    "i'll ",
    "i will ",
    "i'm going to ",
    "i am going to ",
    "i need to ",
    "i'm ",
    "i am ",
    "time to ",
    "going to ",
];

/// Words that may come before an opening (`Now let me`, `Next, I'll`).
const INTENT_LEADS: &[&str] = &[
    "now", "next", "first", "then", "ok", "okay", "good", "great", "so", "and", "finally", "also",
    "alright", "perfect",
];

/// Words that may come between an opening and its verb (`let me also check`).
const ADVERBS: &[&str] = &[
    "also",
    "now",
    "first",
    "quickly",
    "just",
    "then",
    "next",
    "finally",
    "briefly",
    "still",
    "carefully",
    "actually",
    "properly",
    "directly",
    "again",
];

/// What the agent says it is about to do in `text`, as an activity (`Now let me run the
/// tests.` → `running the tests`): the last sentence that opens so ([`INTENT_OPENINGS`]).
pub fn intent(text: &str) -> Option<String> {
    sentences(text)
        .into_iter()
        .rev()
        .find_map(intent_of)
        .map(|p| cut(&p, PHRASE_MAX))
}

/// The intent of one sentence, if it says one.
fn intent_of(sentence: &str) -> Option<String> {
    // (A typographic apostrophe, as in `I’ll`, reads as `'`.)
    let clean = plain(sentence).replace('’', "'");
    let mut s = clean.trim();
    // Leading words such as `Now,` and `Good.`
    while let Some(rest) = INTENT_LEADS
        .iter()
        .find_map(|l| strip_prefix_ascii(s, l).filter(|rest| rest.starts_with([' ', ','])))
    {
        s = rest.trim_start_matches([' ', ',']);
    }
    let (opening, rest) = INTENT_OPENINGS
        .iter()
        .find_map(|o| strip_prefix_ascii(s, o).map(|rest| (*o, rest)))?;
    let rest = rest.trim();
    // `I'm` opens an intent only with a verb in -ing (`I'm checking`, not `I'm sure`).
    if matches!(opening, "i'm " | "i am ") {
        let verb = rest.split_whitespace().next().unwrap_or_default();
        if !verb.to_lowercase().ends_with("ing") {
            return None;
        }
    }
    let rest = rest.trim_end_matches(['.', ':', '!', '?', ',', ';', ' ']);
    if rest.split_whitespace().count() < 2 {
        return None;
    }
    Some(gerund(rest))
}

/// `s` after `prefix` (ASCII, lowercase), in any case, if it starts with it. Compared on `s`'s
/// own bytes: lowercasing first could change where its characters start (`K`, the Kelvin
/// sign, lowercases to the one-byte `k`), and slicing there would panic.
fn strip_prefix_ascii<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// `phrase`, an imperative (`Run the tests`, `also check app.rs`), as what is being done
/// (`running the tests`, `checking app.rs`): its verb in -ing, after any adverbs it starts with,
/// lowercase unless it is an acronym.
pub fn gerund(phrase: &str) -> String {
    let mut words: Vec<&str> = phrase.split_whitespace().collect();
    while words.len() > 1 && ADVERBS.contains(&words[0].to_lowercase().as_str()) {
        words.remove(0);
    }
    let Some(first) = words.first() else {
        return String::new();
    };
    // No verb to turn: an acronym (`CI checks`), a number, a path.
    let acronym = first.chars().count() > 1 && first.chars().all(|c| c.is_ascii_uppercase());
    let verb = lower_first(first);
    let ing = if acronym || !verb.chars().all(|c| c.is_ascii_alphabetic()) || verb.ends_with("ing")
    {
        verb.clone()
    } else {
        ing_form(&verb)
    };
    let mut out = ing;
    for w in &words[1..] {
        out.push(' ');
        out.push_str(w);
    }
    out
}

/// `word` with its first letter in lowercase, unless the word is an acronym (`CI`, `TUI`).
fn lower_first(word: &str) -> String {
    let mut chars = word.chars();
    let (Some(a), b) = (chars.next(), chars.next()) else {
        return word.to_string();
    };
    if b.is_some_and(char::is_uppercase) {
        return word.to_string();
    }
    let mut out: String = a.to_lowercase().collect();
    out.extend(word.chars().skip(1));
    out
}

/// Verbs whose last consonant doubles before -ing.
const DOUBLED: &[&str] = &[
    "run", "get", "set", "put", "stop", "cut", "shut", "plan", "commit", "submit", "begin", "skip",
    "ship", "drop", "swap", "scan", "map", "tag", "log", "dig", "pin", "grep", "strip", "trim",
    "wrap", "chop", "prep", "rerun", "reset", "upset", "rip", "sit", "spin", "stub", "tap", "plug",
    "debug", "zip", "unzip", "admit", "permit", "refer", "omit",
];

/// The -ing form of verb `v` (lowercase).
fn ing_form(v: &str) -> String {
    if DOUBLED.contains(&v) {
        let last = v.chars().last().unwrap_or_default();
        return format!("{v}{last}ing");
    }
    match v {
        "be" | "see" | "flee" | "agree" | "free" | "guarantee" => return format!("{v}ing"),
        "lie" | "die" | "tie" => return format!("{}ying", &v[..v.len() - 2]),
        _ => {}
    }
    if v.len() > 2
        && v.ends_with('e')
        && !v.ends_with("ee")
        && !v.ends_with("ye")
        && !v.ends_with("oe")
    {
        return format!("{}ing", &v[..v.len() - 1]);
    }
    format!("{v}ing")
}

/// `text`'s sentences: split after `.`, `!`, `?` or `:` followed by a space, and at line
/// breaks.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut start = 0;
        let bytes = line.as_bytes();
        for (i, c) in line.char_indices() {
            let end = matches!(c, '.' | '!' | '?' | ':');
            let next_space = bytes.get(i + 1).is_none_or(|b| *b == b' ');
            if end && next_space {
                let s = line[start..=i].trim();
                if !s.is_empty() {
                    out.push(s);
                }
                start = i + 1;
            }
        }
        let s = line[start..].trim();
        if !s.is_empty() {
            out.push(s);
        }
    }
    out
}

/// `s` without markdown's emphasis and code marks, list and heading markers.
fn plain(s: &str) -> String {
    let s = s.trim_start_matches(['#', '>', '-', '*', '+', ' ']);
    s.replace("**", "").replace('`', "").replace("__", "")
}

/// What a finished turn came to, from the start of the agent's last message: its first
/// sentence (with the next when it is a word or two, as `Done.`), as plain text.
pub fn result(message_head: &str) -> Option<String> {
    // Not its headings, rules, code fences or tags (an orchestrator's `<thread name="…">`).
    let text = message_head
        .lines()
        .filter(|l| {
            let l = l.trim_start();
            !l.starts_with('#')
                && !l.starts_with("```")
                && !l.starts_with("---")
                && !l.starts_with('<')
        })
        .collect::<Vec<_>>()
        .join("\n");
    let sentences = sentences(&text);
    let mut it = sentences.iter().map(|s| plain(s));
    let mut out = it.next()?.trim().to_string();
    if out.split_whitespace().count() <= 2 {
        if let Some(next) = it.next() {
            out = format!("{out} {}", next.trim());
        }
    }
    let out = out.trim_end_matches(':').trim().to_string();
    (!out.is_empty()).then(|| cut(&out, PHRASE_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_say_what_they_do() {
        for (cmd, phrase) in [
            ("cargo test -p twr-tui", "running tests"),
            ("cd /x && cargo fmt --all && cargo clippy -- -D warnings && cargo test 2>&1 | tail -20", "running tests"),
            ("cargo fmt --all --check && cargo clippy --workspace", "linting"),
            ("RUST_LOG=debug timeout 600 cargo nextest run", "running tests"),
            ("env -u TWR_AGENT cargo test --workspace", "running tests"),
            (
                "systemd-run --user --wait --pipe --collect -q --working-directory=$PWD \
                 --property=UnsetEnvironment=\"TWR_AGENT TWR_THREAD\" ~/.cargo/bin/cargo test --workspace",
                "running tests",
            ),
            ("cargo build --release", "building"),
            ("npm run lint", "linting"),
            ("pnpm test -- --watch=false", "running tests"),
            ("npm install", "installing dependencies"),
            ("make", "building"),
            ("make test-unit", "running tests"),
            ("python3 -m pytest tests/", "running tests"),
            ("python scripts/gen.py --out x", "running gen.py"),
            ("go test ./...", "running tests"),
            ("git commit -m \"fix cargo test flake\"", "committing"),
            ("git -C /repo status --short", "reviewing changes"),
            ("git log --oneline | head", "reading the history"),
            ("gh pr create --fill", "opening a pull request"),
            ("rg -n \"fn main\" crates", "searching the code"),
            ("sed -n 1,80p crates/twr-tui/src/ui.rs", "reading ui.rs"),
            ("sed -i 's/a/b/' src/lib.rs", "editing lib.rs"),
            ("cat Cargo.toml", "reading Cargo.toml"),
            ("ls -la", "looking around"),
            ("curl -s https://docs.rs/ratatui/latest", "fetching docs.rs"),
            ("echo done", "running a command"),
            ("./target/debug/twr --help", "using twr"),
            ("./target/release/app --port 3000", "running app"),
            ("bash -c \"cd x && cargo test\"", "running tests"),
            ("bash -lc 'git status'", "reviewing changes"),
            ("bash -c \"cd \\\"my dir\\\" && cargo test\"", "running tests"),
            ("cat my\\ notes.md", "reading my notes.md"),
            ("sh scripts/release.sh", "running release.sh"),
            // Separators inside quotes or escaped separate nothing.
            ("git commit -m 'fix; cargo test'", "committing"),
            ("git commit -m \"a && cargo test | b\"", "committing"),
            ("echo a \\; cargo test", "running a command"),
            ("cargo build & cargo test", "running tests"),
            ("cargo test 2>&1 | tee out.log", "running tests"),
            ("make &> build.log", "building"),
            ("cat notes.md > copy.md", "reading notes.md"),
            ("cat log | grep error || cargo build", "building"),
        ] {
            assert_eq!(command_phrase(cmd), phrase, "{cmd}");
        }
    }

    #[test]
    fn tool_calls_say_what_they_do() {
        for (tool, summary, phrase) in [
            (
                "Read",
                "/home/u/src/twr/crates/twr-tui/src/app.rs",
                "reading app.rs",
            ),
            ("Edit", "/home/u/src/ui.rs", "editing ui.rs"),
            ("Write", "docs/tui.md", "writing tui.md"),
            ("apply_patch", "src/a.rs, src/b.rs", "editing a.rs, b.rs"),
            (
                "apply_patch",
                "edit 5 files: src/a.rs, src/b.rs, src/c.rs, src/d.rs and 1 more",
                "editing a.rs, b.rs, c.rs, d.rs and 1 more",
            ),
            (
                "apply_patch",
                "edit 1 file: crates/x/src/ui.rs",
                "editing ui.rs",
            ),
            ("read", "Read src/main.rs", "reading main.rs"),
            (
                "Grep",
                "fn scroll_line in crates/twr-tui",
                "searching for fn scroll_line",
            ),
            (
                "WebSearch",
                "codex login flow",
                "researching codex login flow",
            ),
            (
                "WebFetch",
                "https://docs.rs/ratatui/latest/ratatui/",
                "reading docs.rs",
            ),
            (
                "Task",
                "Explore key handling",
                "delegating: Explore key handling",
            ),
            ("TodoWrite", "4 todos", "planning"),
            (
                "mcp__playwright__browser_click",
                "{}",
                "using playwright browser_click",
            ),
            ("Frobnicate", "", "using Frobnicate"),
            ("Bash", "cargo test", "running tests"),
            ("shell", "git push origin HEAD", "pushing"),
        ] {
            assert_eq!(tool_phrase(tool, summary, None), phrase, "{tool} {summary}");
        }
        // The purpose the agent gave a call says it best.
        assert_eq!(
            tool_phrase("Bash", "cargo test -p twr-tui", Some("Run the TUI tests")),
            "running the TUI tests"
        );
    }

    #[test]
    fn imperatives_read_as_what_is_being_done() {
        for (imperative, gerund_form) in [
            ("Run the tests", "running the tests"),
            (
                "check the scroll anchoring",
                "checking the scroll anchoring",
            ),
            ("also make sure it builds", "making sure it builds"),
            ("Commit part 1", "committing part 1"),
            ("Verify the fix", "verifying the fix"),
            ("Use the cache", "using the cache"),
            ("see what changed", "seeing what changed"),
            ("CI checks", "CI checks"),
            (
                "Implementing scroll anchoring",
                "implementing scroll anchoring",
            ),
        ] {
            assert_eq!(gerund(imperative), gerund_form, "{imperative}");
        }
    }

    #[test]
    fn the_agent_says_what_it_is_about_to_do() {
        for (text, said) in [
            (
                "Let me look at the TUI key handling.",
                Some("looking at the TUI key handling"),
            ),
            (
                "The build passes. Now let me run the tests:",
                Some("running the tests"),
            ),
            (
                "Good, now I'll **commit** part 1.",
                Some("committing part 1"),
            ),
            (
                "I'm checking the hub's activity module.",
                Some("checking the hub's activity module"),
            ),
            ("I'm sure this works.", None),
            // Characters whose lowercase takes fewer bytes do not move where it is cut.
            ("o\u{212A}, I'll run the tests.", None),
            ("Now I’ll run the tests.", Some("running the tests")),
            ("All three parts are committed.", None),
            ("Let me see.", None),
        ] {
            assert_eq!(intent(text).as_deref(), said, "{text}");
        }
    }

    #[test]
    fn a_result_is_the_first_sentence_of_the_last_message() {
        assert_eq!(
            result("All three parts are committed. The suite passes.").as_deref(),
            Some("All three parts are committed.")
        );
        assert_eq!(
            result("Done. Scroll anchoring now keeps the view.").as_deref(),
            Some("Done. Scroll anchoring now keeps the view.")
        );
        assert_eq!(
            result("## Summary\n\nThe fix is in.").as_deref(),
            Some("The fix is in.")
        );
        assert_eq!(
            result("<thread name=\"api\">\napi is done.\n</thread>").as_deref(),
            Some("api is done.")
        );
        assert_eq!(result("").as_deref(), None);
    }

    #[test]
    fn cut_keeps_whole_words() {
        assert_eq!(
            cut("running the whole test suite", 20),
            "running the whole…"
        );
        assert_eq!(cut("short", 20), "short");
    }
}

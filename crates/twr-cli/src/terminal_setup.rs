//! `twr terminal-setup`: makes the terminal send Shift-Enter as Alt-Enter (`ESC CR`), which
//! the TUI reads as a new line everywhere, over mosh and SSH too.
//!
//! The TUI tells Shift-Enter from Enter when the terminal speaks the kitty keyboard protocol,
//! but the protocol has to be asked for, and mosh never passes the request on: over mosh every
//! terminal sends Shift-Enter as a plain Enter. A key binding in the terminal itself crosses
//! mosh, as mosh passes the bytes a key sends unchanged. Claude Code's `/terminal-setup` binds
//! Shift-Enter to `ESC CR` in VS Code (and its forks), Alacritty and Zed, and turns on Option as
//! Meta in Terminal.app; this does the same, so a terminal set up for Claude Code is set up for
//! twr already.
//!
//! A terminal's settings live on the machine it runs on. Over SSH or mosh this edits nothing:
//! it prints what to change there. Locally it edits only files it can change safely (a JSON
//! array, a TOML file that still parses, a line-based config), backs each one up first, and
//! leaves any Shift-Enter binding already there as it is. iTerm2, Terminal.app and WezTerm keep
//! their settings in a property list or a Lua program: it prints the steps for those.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// What the binding sends: Alt-Enter, `ESC CR`.
pub const SEQUENCE: &str = "\x1b\r";

/// The terminals `twr terminal-setup` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    AppleApp,
    ITerm2,
    Ghostty,
    Kitty,
    WezTerm,
    Alacritty,
    VsCode(Editor),
    Zed,
}

/// VS Code and the editors built on it, whose keybindings.json takes the same binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    Code,
    Insiders,
    Cursor,
    Windsurf,
    Codium,
}

impl Editor {
    /// Its folder under the configuration directory.
    fn dir(self) -> &'static str {
        match self {
            Editor::Code => "Code",
            Editor::Insiders => "Code - Insiders",
            Editor::Cursor => "Cursor",
            Editor::Windsurf => "Windsurf",
            Editor::Codium => "VSCodium",
        }
    }
}

/// Every terminal, in the order `--print` lists them.
pub const ALL: &[Terminal] = &[
    Terminal::AppleApp,
    Terminal::ITerm2,
    Terminal::Ghostty,
    Terminal::Kitty,
    Terminal::WezTerm,
    Terminal::Alacritty,
    Terminal::VsCode(Editor::Code),
    Terminal::Zed,
];

impl Terminal {
    /// Its name, as `--terminal` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Terminal::AppleApp => "terminal.app",
            Terminal::ITerm2 => "iterm2",
            Terminal::Ghostty => "ghostty",
            Terminal::Kitty => "kitty",
            Terminal::WezTerm => "wezterm",
            Terminal::Alacritty => "alacritty",
            Terminal::VsCode(Editor::Code) => "vscode",
            Terminal::VsCode(Editor::Insiders) => "vscode-insiders",
            Terminal::VsCode(Editor::Cursor) => "cursor",
            Terminal::VsCode(Editor::Windsurf) => "windsurf",
            Terminal::VsCode(Editor::Codium) => "vscodium",
            Terminal::Zed => "zed",
        }
    }

    /// How people call it.
    pub fn title(self) -> &'static str {
        match self {
            Terminal::AppleApp => "Terminal.app",
            Terminal::ITerm2 => "iTerm2",
            Terminal::Ghostty => "Ghostty",
            Terminal::Kitty => "kitty",
            Terminal::WezTerm => "WezTerm",
            Terminal::Alacritty => "Alacritty",
            Terminal::VsCode(Editor::Code) => "VS Code",
            Terminal::VsCode(Editor::Insiders) => "VS Code Insiders",
            Terminal::VsCode(Editor::Cursor) => "Cursor",
            Terminal::VsCode(Editor::Windsurf) => "Windsurf",
            Terminal::VsCode(Editor::Codium) => "VSCodium",
            Terminal::Zed => "Zed",
        }
    }

    /// The terminal `--terminal` names (case aside, `-`, `_` and `.` optional).
    pub fn parse(s: &str) -> Option<Terminal> {
        let key: String = s
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | '.' | ' '))
            .flat_map(char::to_lowercase)
            .collect();
        Some(match key.as_str() {
            "terminal" | "terminalapp" | "appleterminal" => Terminal::AppleApp,
            "iterm" | "iterm2" | "itermapp" => Terminal::ITerm2,
            "ghostty" => Terminal::Ghostty,
            "kitty" => Terminal::Kitty,
            "wezterm" => Terminal::WezTerm,
            "alacritty" => Terminal::Alacritty,
            "vscode" | "code" => Terminal::VsCode(Editor::Code),
            "vscodeinsiders" | "codeinsiders" | "insiders" => Terminal::VsCode(Editor::Insiders),
            "cursor" => Terminal::VsCode(Editor::Cursor),
            "windsurf" => Terminal::VsCode(Editor::Windsurf),
            "vscodium" | "codium" => Terminal::VsCode(Editor::Codium),
            "zed" => Terminal::Zed,
            _ => return None,
        })
    }
}

/// Where twr runs, as far as the environment tells: what is between it and the terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Place {
    /// The terminal, when the environment names it.
    pub terminal: Option<Terminal>,
    /// It runs over SSH or mosh: the terminal is on another machine (`"ssh"`, `"mosh"`).
    pub remote: Option<&'static str>,
    /// It runs inside tmux or screen, which hide the terminal around them.
    pub multiplexer: Option<&'static str>,
    /// macOS (settings under `~/Library`), else the XDG folders.
    pub mac: bool,
    /// The home directory.
    pub home: PathBuf,
    /// `XDG_CONFIG_HOME`, when set.
    pub xdg_config: Option<PathBuf>,
    /// `KITTY_CONFIG_DIRECTORY`, when set.
    pub kitty_config: Option<PathBuf>,
}

/// The terminal the environment `var` describes (`None`: it doesn't say). Inside tmux or
/// screen `TERM_PROGRAM` is the multiplexer's, but the variables a terminal sets for the
/// shells it starts are still there.
pub fn detect(var: &impl Fn(&str) -> Option<String>) -> Option<Terminal> {
    let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
    let program = var("TERM_PROGRAM").unwrap_or_default();
    match program.as_str() {
        "Apple_Terminal" => return Some(Terminal::AppleApp),
        "iTerm.app" => return Some(Terminal::ITerm2),
        "ghostty" => return Some(Terminal::Ghostty),
        "WezTerm" => return Some(Terminal::WezTerm),
        "zed" => return Some(Terminal::Zed),
        "vscode" => return Some(Terminal::VsCode(vscode_flavor(var))),
        _ => {}
    }
    if set("GHOSTTY_RESOURCES_DIR") {
        Some(Terminal::Ghostty)
    } else if set("KITTY_WINDOW_ID") {
        Some(Terminal::Kitty)
    } else if set("WEZTERM_EXECUTABLE") || set("WEZTERM_PANE") {
        Some(Terminal::WezTerm)
    } else if set("ALACRITTY_WINDOW_ID") || set("ALACRITTY_LOG") || set("ALACRITTY_SOCKET") {
        Some(Terminal::Alacritty)
    } else if set("ITERM_SESSION_ID") {
        Some(Terminal::ITerm2)
    } else if set("VSCODE_GIT_ASKPASS_MAIN") || set("VSCODE_INJECTION") {
        Some(Terminal::VsCode(vscode_flavor(var)))
    } else {
        None
    }
}

/// Which editor built on VS Code the integrated terminal belongs to: they all say
/// `TERM_PROGRAM=vscode`, and the path of the editor's own git helper names it.
fn vscode_flavor(var: &impl Fn(&str) -> Option<String>) -> Editor {
    let askpass = var("VSCODE_GIT_ASKPASS_MAIN")
        .unwrap_or_default()
        .to_lowercase();
    if var("CURSOR_TRACE_ID").is_some() || askpass.contains("cursor") {
        Editor::Cursor
    } else if askpass.contains("windsurf") {
        Editor::Windsurf
    } else if askpass.contains("codium") {
        Editor::Codium
    } else if askpass.contains("insiders")
        || var("TERM_PROGRAM_VERSION").is_some_and(|v| v.ends_with("-insider"))
    {
        Editor::Insiders
    } else {
        Editor::Code
    }
}

/// [`Place`] from this process's environment; `mosh` says whether mosh runs above it.
pub fn place(mosh: bool) -> Place {
    let var = |k: &str| std::env::var(k).ok();
    let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
    let remote = if mosh {
        Some("mosh")
    } else if set("SSH_CONNECTION") || set("SSH_TTY") || set("SSH_CLIENT") {
        Some("ssh")
    } else {
        None
    };
    let multiplexer = if set("TMUX") {
        Some("tmux")
    } else if set("STY") {
        Some("screen")
    } else {
        None
    };
    Place {
        terminal: detect(&var),
        remote,
        multiplexer,
        mac: cfg!(target_os = "macos"),
        home: var("HOME").map(PathBuf::from).unwrap_or_default(),
        xdg_config: var("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        kitty_config: var("KITTY_CONFIG_DIRECTORY")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
    }
}

impl Place {
    /// The XDG configuration directory.
    fn config_dir(&self) -> PathBuf {
        self.xdg_config
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
    }

    /// The files `t` reads its settings from, the one to write first (`None`: none twr edits).
    pub fn files(&self, t: Terminal) -> Vec<PathBuf> {
        let config = self.config_dir();
        let support = self.home.join("Library/Application Support");
        match t {
            Terminal::VsCode(e) => {
                let base = if self.mac { support } else { config };
                vec![base.join(e.dir()).join("User/keybindings.json")]
            }
            Terminal::Zed => vec![config.join("zed/keymap.json")],
            Terminal::Alacritty => vec![
                config.join("alacritty/alacritty.toml"),
                self.home.join(".alacritty.toml"),
            ],
            Terminal::Ghostty => {
                let mut v = vec![
                    config.join("ghostty/config"),
                    config.join("ghostty/config.ghostty"),
                ];
                if self.mac {
                    v.push(support.join("com.mitchellh.ghostty/config"));
                    v.push(support.join("com.mitchellh.ghostty/config.ghostty"));
                }
                v
            }
            Terminal::Kitty => vec![self
                .kitty_config
                .clone()
                .unwrap_or_else(|| config.join("kitty"))
                .join("kitty.conf")],
            Terminal::ITerm2 | Terminal::AppleApp | Terminal::WezTerm => Vec::new(),
        }
    }

    /// The file to edit for `t`: the first of [`Place::files`] that exists, else the first.
    pub fn file(&self, t: Terminal) -> Option<PathBuf> {
        let files = self.files(t);
        files
            .iter()
            .find(|p| p.is_file())
            .or(files.first())
            .cloned()
    }
}

/// What to do by hand for `t`, as lines to print.
pub fn instructions(t: Terminal) -> Vec<String> {
    let lines: &[&str] = match t {
        Terminal::AppleApp => &[
            "Terminal.app can't change what Shift-Enter sends. Use Option-Enter instead:",
            "  Settings → Profiles → (your profile) → Keyboard → check \"Use Option as Meta key\".",
            "  Option-Enter then sends ESC CR, which twr reads as a new line (over mosh too).",
            "  Or run: defaults write com.apple.Terminal \"Window Settings\" ... (no safe one-liner:",
            "  use the Settings window).",
        ],
        Terminal::ITerm2 => &[
            "iTerm2: Settings → Profiles → (your profile) → Keys → Key Mappings → +",
            "  Keyboard Shortcut: press Shift-Enter",
            "  Action: Send Hex Codes, 0x1b 0x0d",
            "  (iTerm2 tells Shift-Enter apart by itself on this machine; the mapping is what",
            "  makes it work over mosh, which doesn't pass the keyboard protocol on.)",
        ],
        Terminal::Ghostty => &[
            "Ghostty: add this line to its config (~/.config/ghostty/config),",
            "then reload the config (Cmd-Shift-, on macOS, Ctrl-Shift-, on Linux):",
            "  keybind = shift+enter=text:\\x1b\\r",
        ],
        Terminal::Kitty => &[
            "kitty: add this line to kitty.conf (~/.config/kitty/kitty.conf),",
            "then reload the config (Ctrl-Shift-F5, Cmd-Ctrl-, on macOS):",
            "  map shift+enter send_text all \\x1b\\r",
        ],
        Terminal::WezTerm => &[
            "WezTerm: add the binding to the keys in your .wezterm.lua, before `return config`:",
            "  config.keys = config.keys or {}",
            "  table.insert(config.keys, {",
            "    key = 'Enter', mods = 'SHIFT',",
            "    action = wezterm.action.SendString '\\x1b\\r',",
            "  })",
        ],
        Terminal::Alacritty => &[
            "Alacritty: add this to alacritty.toml (~/.config/alacritty/alacritty.toml):",
            "  [[keyboard.bindings]]",
            "  key = \"Return\"",
            "  mods = \"Shift\"",
            "  chars = \"\\u001B\\r\"",
        ],
        Terminal::VsCode(_) => &[
            "VS Code (and Cursor, Windsurf, VSCodium): Command Palette → \"Preferences: Open",
            "Keyboard Shortcuts (JSON)\", and add to the list:",
            "  {",
            "    \"key\": \"shift+enter\",",
            "    \"command\": \"workbench.action.terminal.sendSequence\",",
            "    \"args\": { \"text\": \"\\u001b\\r\" },",
            "    \"when\": \"terminalFocus\"",
            "  }",
        ],
        Terminal::Zed => &[
            "Zed: open the keymap (zed: open keymap) and add to the list:",
            "  { \"context\": \"Terminal\", \"bindings\": { \"shift-enter\": [\"terminal::SendText\", \"\\u001b\\r\"] } }",
        ],
    };
    lines.iter().map(|l| (*l).to_string()).collect()
}

/// What [`edit`] did, or found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It added the binding; the file was backed up to the path (`None`: it didn't exist).
    Added {
        file: PathBuf,
        backup: Option<PathBuf>,
    },
    /// Shift-Enter already sends `ESC CR` there (Claude Code's setup, or an earlier run).
    Already { file: PathBuf },
    /// Shift-Enter has another binding there: left as is.
    Other { file: PathBuf },
    /// The file can't be changed safely (comments in JSON, TOML that wouldn't parse after):
    /// the reason; the instructions say what to add by hand.
    Unsafe { file: PathBuf, why: String },
    /// twr edits no file of this terminal: print the instructions.
    Manual,
}

/// Adds the Shift-Enter binding for `t` to its settings file in `place` (backed up first),
/// unless one is there. Edits nothing for a terminal without a file twr edits.
pub fn edit(place: &Place, t: Terminal) -> anyhow::Result<Outcome> {
    let Some(file) = place.file(t) else {
        return Ok(Outcome::Manual);
    };
    let old = match std::fs::read_to_string(&file) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let text = old.clone().unwrap_or_default();
    let change = match t {
        Terminal::VsCode(_) => vscode(&text),
        Terminal::Zed => zed(&text),
        Terminal::Alacritty => alacritty(&text),
        Terminal::Ghostty => lines(
            &text,
            |l| {
                let l = l.trim_start();
                l.starts_with("keybind") && l.replace(' ', "").contains("=shift+enter=")
            },
            |l| l.contains("\\x1b\\r") || l.contains("\\e\\r"),
            "keybind = shift+enter=text:\\x1b\\r",
        ),
        Terminal::Kitty => lines(
            &text,
            |l| {
                let mut w = l.split_whitespace();
                w.next() == Some("map") && w.next() == Some("shift+enter")
            },
            |l| l.contains("\\x1b\\r") || l.contains("\\e\\r"),
            "map shift+enter send_text all \\x1b\\r",
        ),
        Terminal::ITerm2 | Terminal::AppleApp | Terminal::WezTerm => return Ok(Outcome::Manual),
    };
    let new = match change {
        Change::Already => return Ok(Outcome::Already { file }),
        Change::Other => return Ok(Outcome::Other { file }),
        Change::Unsafe(why) => return Ok(Outcome::Unsafe { file, why }),
        Change::Write(new) => new,
    };
    let backup = match old {
        Some(_) => {
            let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let mut name = file.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".twr-backup-{stamp}"));
            let backup = file.with_file_name(name);
            std::fs::copy(&file, &backup)
                .with_context(|| format!("backing up {}", file.display()))?;
            Some(backup)
        }
        None => {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            None
        }
    };
    write_replacing(&file, &new)?;
    Ok(Outcome::Added { file, backup })
}

/// Writes `text` to `file` through a temporary file beside it, renamed over it, keeping the
/// old file's permissions: a crash leaves the old file or the new one, never half of one.
fn write_replacing(file: &Path, text: &str) -> anyhow::Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".twr-new");
    let tmp = dir.join(name);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(file) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))?;
    Ok(())
}

/// What a settings file needs.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    Already,
    Other,
    Unsafe(String),
    Write(String),
}

/// A line-based config (Ghostty, kitty): `binding` says whether a line binds Shift-Enter,
/// `ours` whether that binding sends `ESC CR`; otherwise `line` is added at the end.
fn lines(
    text: &str,
    binding: impl Fn(&str) -> bool,
    ours: impl Fn(&str) -> bool,
    line: &str,
) -> Change {
    let bound: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| binding(l))
        .collect();
    if bound.iter().any(|l| ours(l)) {
        return Change::Already;
    }
    if !bound.is_empty() {
        return Change::Other;
    }
    let mut new = text.to_string();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str("# Shift-Enter starts a new line in twr and Claude Code, over mosh and SSH too\n");
    new.push_str(line);
    new.push('\n');
    Change::Write(new)
}

/// A JSON list of key bindings (VS Code's keybindings.json, Zed's keymap.json): `None` for an
/// empty file; refused when it isn't plain JSON (comments, trailing commas: rewriting it would
/// drop them).
fn json_list(text: &str, what: &str) -> Result<Vec<serde_json::Value>, Change> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Array(v)) => Ok(v),
        Ok(_) => Err(Change::Unsafe(format!("{what} isn't a list of bindings"))),
        Err(_) => Err(Change::Unsafe(format!(
            "{what} has comments or isn't plain JSON, and twr won't rewrite it"
        ))),
    }
}

/// Pretty JSON for a key bindings list, with a final line break.
fn json_text(list: &[serde_json::Value]) -> String {
    let mut s = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".into());
    s.push('\n');
    s
}

/// VS Code's keybindings.json: Claude Code's binding, unless a terminal Shift-Enter one is
/// there.
fn vscode(text: &str) -> Change {
    let mut list = match json_list(text, "keybindings.json") {
        Ok(l) => l,
        Err(c) => return c,
    };
    let shift_enter: Vec<&serde_json::Value> = list
        .iter()
        .filter(|b| {
            b["key"]
                .as_str()
                .is_some_and(|k| k.replace(' ', "").eq_ignore_ascii_case("shift+enter"))
                && b["when"]
                    .as_str()
                    .is_some_and(|w| w.contains("terminalFocus"))
        })
        .collect();
    if shift_enter.iter().any(|b| {
        b["command"] == "workbench.action.terminal.sendSequence" && b["args"]["text"] == SEQUENCE
    }) {
        return Change::Already;
    }
    if !shift_enter.is_empty() {
        return Change::Other;
    }
    list.push(serde_json::json!({
        "key": "shift+enter",
        "command": "workbench.action.terminal.sendSequence",
        "args": { "text": SEQUENCE },
        "when": "terminalFocus",
    }));
    Change::Write(json_text(&list))
}

/// Zed's keymap.json: Claude Code's binding in the Terminal context, unless one binds
/// shift-enter there.
fn zed(text: &str) -> Change {
    let mut list = match json_list(text, "keymap.json") {
        Ok(l) => l,
        Err(c) => return c,
    };
    let bound: Vec<&serde_json::Value> = list
        .iter()
        .filter(|b| {
            b["context"]
                .as_str()
                .is_some_and(|c| c.contains("Terminal"))
        })
        .filter_map(|b| b["bindings"].get("shift-enter"))
        .collect();
    if bound
        .iter()
        .any(|v| **v == serde_json::json!(["terminal::SendText", SEQUENCE]))
    {
        return Change::Already;
    }
    if !bound.is_empty() {
        return Change::Other;
    }
    list.push(serde_json::json!({
        "context": "Terminal",
        "bindings": { "shift-enter": ["terminal::SendText", SEQUENCE] },
    }));
    Change::Write(json_text(&list))
}

/// Alacritty's TOML: a `[[keyboard.bindings]]` table added at the end, checked by parsing the
/// result (a file that writes `bindings = [...]` inline would not take it).
fn alacritty(text: &str) -> Change {
    let parsed: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(_) => {
            return Change::Unsafe("alacritty.toml doesn't parse as TOML".into());
        }
    };
    let bindings = parsed
        .get("keyboard")
        .and_then(|k| k.get("bindings"))
        .and_then(|b| b.as_array());
    let shift_enter: Vec<&toml::Value> = bindings
        .into_iter()
        .flatten()
        .filter(|b| {
            let key = b.get("key").and_then(|k| k.as_str()).unwrap_or_default();
            let mods = b.get("mods").and_then(|m| m.as_str()).unwrap_or_default();
            matches!(key.to_lowercase().as_str(), "return" | "enter")
                && mods.eq_ignore_ascii_case("shift")
        })
        .collect();
    if shift_enter
        .iter()
        .any(|b| b.get("chars").and_then(|c| c.as_str()) == Some(SEQUENCE))
    {
        return Change::Already;
    }
    if !shift_enter.is_empty() {
        return Change::Other;
    }
    let mut new = text.to_string();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    if !new.is_empty() {
        new.push('\n');
    }
    new.push_str(
        "# Shift-Enter starts a new line in twr and Claude Code, over mosh and SSH too\n\
         [[keyboard.bindings]]\nkey = \"Return\"\nmods = \"Shift\"\nchars = \"\\u001B\\r\"\n",
    );
    match new.parse::<toml::Table>() {
        Ok(_) => Change::Write(new),
        Err(_) => Change::Unsafe(
            "alacritty.toml lists its bindings in a way twr can't add to (an inline array)".into(),
        ),
    }
}

/// `twr terminal-setup`: the lines to print, and whether it failed. `terminal` is what
/// `--terminal` named; `print` only prints; `agent` refuses edits (a thread's agent never
/// changes the user's terminal).
pub fn run(
    place: &Place,
    terminal: Option<Terminal>,
    print: bool,
    agent: Option<&str>,
) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let target = terminal.or(place.terminal);
    out.push(
        "twr reads Alt-Enter (ESC CR) as a new line everywhere. This makes your terminal send it \
         for Shift-Enter, as Claude Code's /terminal-setup does, so Shift-Enter starts a new \
         line over mosh and SSH too."
            .to_string(),
    );
    let why_not = if print {
        None
    } else if let Some(r) = place.remote {
        Some(format!(
            "This runs over {r}: your terminal's settings are on the machine it runs on. Change \
             them there (or run `twr terminal-setup` there):"
        ))
    } else if let Some(a) = agent {
        Some(format!(
            "This runs for {a}: an agent doesn't change your terminal's settings. To change them:"
        ))
    } else if let Some(m) = place.multiplexer {
        // A session of tmux or screen may be attached from another machine: its terminal's
        // settings may not be here.
        Some(format!(
            "Inside {m} twr can't tell which machine your terminal runs on, so it changes \
             nothing: run `twr terminal-setup` outside {m}, or change it by hand:"
        ))
    } else if target.is_none() {
        Some(
            "twr can't tell which terminal this is: name it with --terminal \
             (terminal.app, iterm2, ghostty, kitty, wezterm, alacritty, vscode, cursor, \
             windsurf, vscodium, zed), or change it by hand:"
                .to_string(),
        )
    } else {
        None
    };
    if let Some(why) = why_not {
        out.push(String::new());
        out.push(why);
        let list: Vec<Terminal> = match target {
            Some(t) => vec![t],
            None => ALL.to_vec(),
        };
        for t in list {
            out.push(String::new());
            out.extend(instructions(t));
        }
        return (out, false);
    }
    let Some(t) = target else {
        // `print` with nothing named: every terminal.
        for t in ALL {
            out.push(String::new());
            out.extend(instructions(*t));
        }
        return (out, false);
    };
    out.push(String::new());
    if print {
        out.extend(instructions(t));
        return (out, false);
    }
    match edit(place, t) {
        Ok(Outcome::Added { file, backup }) => {
            out.push(format!(
                "{}: added the Shift-Enter binding to {}",
                t.title(),
                file.display()
            ));
            if let Some(b) = backup {
                out.push(format!("the file as it was is in {}", b.display()));
            }
            out.push(match t {
                Terminal::VsCode(_) | Terminal::Zed => {
                    "It works in new terminals; no restart needed.".to_string()
                }
                _ => format!("Reload {}'s config, or restart it.", t.title()),
            });
            (out, false)
        }
        Ok(Outcome::Already { file }) => {
            out.push(format!(
                "{}: Shift-Enter already sends ESC CR ({}): nothing to do.",
                t.title(),
                file.display()
            ));
            (out, false)
        }
        Ok(Outcome::Other { file }) => {
            out.push(format!(
                "{}: Shift-Enter has another binding in {}; left as it is. To change it by hand:",
                t.title(),
                file.display()
            ));
            out.push(String::new());
            out.extend(instructions(t));
            (out, false)
        }
        Ok(Outcome::Unsafe { file, why }) => {
            out.push(format!(
                "{}: {} ({}). Add the binding by hand:",
                t.title(),
                why,
                file.display()
            ));
            out.push(String::new());
            out.extend(instructions(t));
            (out, false)
        }
        Ok(Outcome::Manual) => {
            out.extend(instructions(t));
            (out, false)
        }
        Err(e) => {
            out.push(format!("{}: {e:#}", t.title()));
            out.push(String::new());
            out.extend(instructions(t));
            (out, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The environment `pairs` as a lookup.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    fn place_in(dir: &Path, mac: bool) -> Place {
        Place {
            home: dir.to_path_buf(),
            mac,
            ..Place::default()
        }
    }

    #[test]
    fn the_environment_names_the_terminal() {
        let d = |pairs: &[(&str, &str)]| detect(&env_of(pairs));
        assert_eq!(
            d(&[("TERM_PROGRAM", "Apple_Terminal")]),
            Some(Terminal::AppleApp)
        );
        assert_eq!(d(&[("TERM_PROGRAM", "iTerm.app")]), Some(Terminal::ITerm2));
        assert_eq!(d(&[("TERM_PROGRAM", "ghostty")]), Some(Terminal::Ghostty));
        assert_eq!(d(&[("TERM_PROGRAM", "WezTerm")]), Some(Terminal::WezTerm));
        assert_eq!(
            d(&[("TERM_PROGRAM", "vscode")]),
            Some(Terminal::VsCode(Editor::Code))
        );
        assert_eq!(
            d(&[
                ("TERM_PROGRAM", "vscode"),
                (
                    "VSCODE_GIT_ASKPASS_MAIN",
                    "/Applications/Cursor.app/Contents/Resources/app/extensions/git/dist/askpass-main.js"
                )
            ]),
            Some(Terminal::VsCode(Editor::Cursor))
        );
        assert_eq!(
            d(&[
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.99.0-insider")
            ]),
            Some(Terminal::VsCode(Editor::Insiders))
        );
        // Inside tmux TERM_PROGRAM is tmux's; the terminal's own variables stay.
        assert_eq!(
            d(&[("TERM_PROGRAM", "tmux"), ("KITTY_WINDOW_ID", "1")]),
            Some(Terminal::Kitty)
        );
        assert_eq!(
            d(&[("TERM_PROGRAM", "tmux"), ("ALACRITTY_LOG", "/tmp/a.log")]),
            Some(Terminal::Alacritty)
        );
        assert_eq!(
            d(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")]),
            Some(Terminal::Ghostty)
        );
        assert_eq!(d(&[("TERM", "xterm-256color")]), None);
        assert_eq!(d(&[]), None);
    }

    #[test]
    fn names_given_with_terminal_parse_loosely() {
        assert_eq!(Terminal::parse("Terminal.app"), Some(Terminal::AppleApp));
        assert_eq!(Terminal::parse("iTerm2"), Some(Terminal::ITerm2));
        assert_eq!(
            Terminal::parse("VS Code"),
            Some(Terminal::VsCode(Editor::Code))
        );
        assert_eq!(
            Terminal::parse("vscode-insiders"),
            Some(Terminal::VsCode(Editor::Insiders))
        );
        assert_eq!(Terminal::parse("hyper"), None);
        for t in ALL {
            assert_eq!(Terminal::parse(t.name()), Some(*t), "{t:?}");
        }
    }

    #[test]
    fn vscode_gets_claude_codes_binding_once() {
        let dir = tempfile::tempdir().unwrap();
        let place = place_in(dir.path(), false);
        let t = Terminal::VsCode(Editor::Code);
        let file = dir.path().join(".config/Code/User/keybindings.json");
        // No file yet: created, no backup.
        assert_eq!(
            edit(&place, t).unwrap(),
            Outcome::Added {
                file: file.clone(),
                backup: None
            }
        );
        let list: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            list,
            serde_json::json!([{
                "key": "shift+enter",
                "command": "workbench.action.terminal.sendSequence",
                "args": {"text": "\u{1b}\r"},
                "when": "terminalFocus",
            }])
        );
        // Run again (or after Claude Code's setup): nothing to do.
        assert_eq!(
            edit(&place, t).unwrap(),
            Outcome::Already { file: file.clone() }
        );
        // On macOS the file is under Application Support.
        let mac = place_in(dir.path(), true);
        assert_eq!(
            mac.file(Terminal::VsCode(Editor::Cursor)).unwrap(),
            dir.path()
                .join("Library/Application Support/Cursor/User/keybindings.json")
        );
    }

    #[test]
    fn vscode_keeps_other_bindings_and_refuses_comments() {
        let dir = tempfile::tempdir().unwrap();
        let place = place_in(dir.path(), false);
        let t = Terminal::VsCode(Editor::Code);
        let file = dir.path().join(".config/Code/User/keybindings.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        // Another binding stays; ours is added after it, and the old file is backed up.
        std::fs::write(&file, r#"[{"key": "ctrl+k", "command": "x"}]"#).unwrap();
        let Outcome::Added {
            backup: Some(backup),
            ..
        } = edit(&place, t).unwrap()
        else {
            panic!("not added");
        };
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            r#"[{"key": "ctrl+k", "command": "x"}]"#
        );
        let list: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["key"], "ctrl+k");
        // A Shift-Enter binding of the user's own: left alone.
        std::fs::write(
            &file,
            r#"[{"key": "shift+enter", "command": "workbench.action.terminal.sendSequence",
                "args": {"text": "\n"}, "when": "terminalFocus"}]"#,
        )
        .unwrap();
        assert_eq!(
            edit(&place, t).unwrap(),
            Outcome::Other { file: file.clone() }
        );
        // Comments (VS Code allows them): not rewritten.
        let commented = "// my keys\n[]\n";
        std::fs::write(&file, commented).unwrap();
        assert!(matches!(edit(&place, t).unwrap(), Outcome::Unsafe { .. }));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), commented);
    }

    #[test]
    fn zed_gets_a_terminal_binding() {
        let dir = tempfile::tempdir().unwrap();
        let place = place_in(dir.path(), true);
        let file = dir.path().join(".config/zed/keymap.json");
        assert!(matches!(
            edit(&place, Terminal::Zed).unwrap(),
            Outcome::Added { .. }
        ));
        let list: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            list[0]["bindings"]["shift-enter"],
            serde_json::json!(["terminal::SendText", "\u{1b}\r"])
        );
        assert_eq!(
            edit(&place, Terminal::Zed).unwrap(),
            Outcome::Already { file }
        );
    }

    #[test]
    fn alacritty_gets_a_table_appended_when_the_file_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let place = place_in(dir.path(), false);
        let file = dir.path().join(".config/alacritty/alacritty.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[font]\nsize = 13").unwrap();
        assert!(matches!(
            edit(&place, Terminal::Alacritty).unwrap(),
            Outcome::Added { .. }
        ));
        let text = std::fs::read_to_string(&file).unwrap();
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(t["font"]["size"].as_integer(), Some(13));
        assert_eq!(
            t["keyboard"]["bindings"][0]["chars"].as_str(),
            Some("\u{1b}\r")
        );
        assert_eq!(
            edit(&place, Terminal::Alacritty).unwrap(),
            Outcome::Already { file: file.clone() }
        );
        // Bindings written as an inline array can't take a table: by hand.
        std::fs::write(
            &file,
            "[keyboard]\nbindings = [{ key = \"N\", mods = \"Control\", action = \"SpawnNewInstance\" }]\n",
        )
        .unwrap();
        assert!(matches!(
            edit(&place, Terminal::Alacritty).unwrap(),
            Outcome::Unsafe { .. }
        ));
        // A Shift-Return binding of the user's: left alone.
        std::fs::write(
            &file,
            "[[keyboard.bindings]]\nkey = \"Return\"\nmods = \"Shift\"\nchars = \"\\n\"\n",
        )
        .unwrap();
        assert_eq!(
            edit(&place, Terminal::Alacritty).unwrap(),
            Outcome::Other { file }
        );
    }

    #[test]
    fn ghostty_and_kitty_get_a_line_once() {
        let dir = tempfile::tempdir().unwrap();
        let place = place_in(dir.path(), false);
        let ghostty = dir.path().join(".config/ghostty/config");
        assert!(matches!(
            edit(&place, Terminal::Ghostty).unwrap(),
            Outcome::Added { .. }
        ));
        assert!(std::fs::read_to_string(&ghostty)
            .unwrap()
            .ends_with("keybind = shift+enter=text:\\x1b\\r\n"));
        assert_eq!(
            edit(&place, Terminal::Ghostty).unwrap(),
            Outcome::Already {
                file: ghostty.clone()
            }
        );
        // Commented out, a binding doesn't count.
        std::fs::write(&ghostty, "# keybind = shift+enter=text:\\n\n").unwrap();
        assert!(matches!(
            edit(&place, Terminal::Ghostty).unwrap(),
            Outcome::Added { .. }
        ));
        std::fs::write(&ghostty, "keybind = shift+enter=text:\\n\n").unwrap();
        assert_eq!(
            edit(&place, Terminal::Ghostty).unwrap(),
            Outcome::Other { file: ghostty }
        );
        let kitty = dir.path().join(".config/kitty/kitty.conf");
        std::fs::create_dir_all(kitty.parent().unwrap()).unwrap();
        std::fs::write(&kitty, "font_size 12").unwrap();
        assert!(matches!(
            edit(&place, Terminal::Kitty).unwrap(),
            Outcome::Added { .. }
        ));
        let text = std::fs::read_to_string(&kitty).unwrap();
        assert!(text.starts_with("font_size 12\n"), "{text}");
        assert!(text.ends_with("map shift+enter send_text all \\x1b\\r\n"));
        assert_eq!(
            edit(&place, Terminal::Kitty).unwrap(),
            Outcome::Already { file: kitty }
        );
    }

    #[test]
    fn remote_agent_and_plist_terminals_edit_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut place = place_in(dir.path(), false);
        place.terminal = Some(Terminal::Ghostty);
        place.remote = Some("mosh");
        let (out, failed) = run(&place, None, false, None);
        assert!(!failed);
        let text = out.join("\n");
        assert!(text.contains("This runs over mosh"), "{text}");
        assert!(text.contains("keybind = shift+enter=text:\\x1b\\r"));
        assert!(!dir.path().join(".config/ghostty/config").exists());
        // Over mosh with no terminal named: every terminal's steps.
        place.terminal = None;
        let text = run(&place, None, false, None).0.join("\n");
        for t in ALL {
            assert!(text.contains(instructions(*t)[0].as_str()), "{t:?}");
        }
        // An agent: nothing edited either.
        place.remote = None;
        let text = run(&place, Some(Terminal::Kitty), false, Some("agent:web"))
            .0
            .join("\n");
        assert!(text.contains("an agent doesn't change"), "{text}");
        assert!(!dir.path().join(".config/kitty/kitty.conf").exists());
        // Inside tmux or screen, a terminal named or found: nothing edited either.
        let mut tmux = place.clone();
        tmux.multiplexer = Some("tmux");
        tmux.terminal = Some(Terminal::Kitty);
        let text = run(&tmux, None, false, None).0.join("\n");
        assert!(text.contains("Inside tmux"), "{text}");
        let text = run(&tmux, Some(Terminal::Ghostty), false, None)
            .0
            .join("\n");
        assert!(text.contains("keybind = shift+enter"), "{text}");
        assert!(!dir.path().join(".config/kitty/kitty.conf").exists());
        assert!(!dir.path().join(".config/ghostty/config").exists());
        // iTerm2 and Terminal.app: the steps.
        let text = run(&place, Some(Terminal::ITerm2), false, None)
            .0
            .join("\n");
        assert!(text.contains("Send Hex Codes, 0x1b 0x0d"), "{text}");
        // Locally, named: done.
        let text = run(&place, Some(Terminal::Kitty), false, None).0.join("\n");
        assert!(text.contains("added the Shift-Enter binding"), "{text}");
        assert!(dir.path().join(".config/kitty/kitty.conf").exists());
    }
}

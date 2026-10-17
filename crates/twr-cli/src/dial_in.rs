//! A dial-in box's side of its setup (docs/runners.md, "Dial-in boxes"): `twr runner
//! install-token <box>` stores the box's token, which `twr box add --dial-in` printed on the
//! hub, in a file only this user can read, and prints the command that runs the runner and a
//! service to paste that keeps it running (a systemd user unit, or a LaunchAgent on a Mac).
//! twr installs no service itself: what runs is what you pasted.
//!
//! The token is read at a prompt that does not show it ([`crate::term::read_hidden_line`]), or
//! from a pipe. It is never echoed, logged or put on a command line, and the commands and
//! services printed hold no secret: they name the token's file.
//!
//! Also here: where `twr runner` takes its token from ([`runner_token`]).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use twr_core::paths::{sh_word, Paths, DEFAULT_WORKSPACE};
use twr_core::runner::Secret;
use twr_runner::client::Stored;
use twr_runner::relay::launchd;

use super::Consent;

/// Largest token read from a pipe.
const MAX_PIPED: u64 = 64 << 10;

/// The heredoc delimiter of the systemd unit's snippet.
const UNIT_END: &str = "TWR_UNIT";

/// The heredoc delimiter of the LaunchAgent's snippet.
const PLIST_END: &str = "TWR_PLIST";

/// What to say when a service can't be written for these paths.
const UNIT_BY_HAND: &str = "write the unit yourself (docs/runners.md)";
const PLIST_BY_HAND: &str = "write the LaunchAgent yourself (docs/runners.md)";

/// `twr runner install-token`'s arguments.
#[derive(Debug)]
pub struct InstallToken {
    pub box_name: String,
    pub hub: Option<String>,
    pub ca: Option<PathBuf>,
    pub token_file: Option<PathBuf>,
    pub yes: bool,
}

/// `twr runner install-token <box>`: checks the arguments, reads the token (a hidden prompt
/// on the terminal, else stdin), stores it ([`twr_runner::client::write_token_file`]: the
/// default file `<config dir>/runner-tokens/<box>.token` in a private directory, or
/// `--token-file`), and prints how to run the runner with it. A file that holds another token
/// is replaced only once confirmed (`--yes` without a terminal): two hubs that name a box
/// alike can't share a machine's generated services, so this is most often the same hub's
/// box with a new token.
pub fn install_token(args: InstallToken) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    super::refuse_agent_action("install a box's runner token")?;
    let name = args.box_name.as_str();
    twr_runner::client::validate_box_name(name)?;
    if let Some(hub) = &args.hub {
        twr_runner::client::check_guided_hub_url(hub).map_err(|e| {
            anyhow!(
                "{e}\n(install-token takes no --allow-insecure: leave out --hub, then start \
                 `twr runner` with the options you need yourself, docs/runners.md)"
            )
        })?;
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let env = |k: &str| std::env::var(k).ok();
    let targets = targets(
        name,
        args.token_file.as_deref(),
        args.ca.as_deref(),
        &env,
        &cwd,
    )?;
    if let Some(ca) = &targets.ca {
        twr_runner::client::tls_connector(ca)?;
    }

    let token = if std::io::stdin().is_terminal() {
        let prompt =
            format!("Token for box {name} (from `twr box add` on the hub; it is not shown): ");
        crate::term::read_hidden_line(&prompt).map_err(|e| {
            if e.to_string() == "cancelled" {
                anyhow!("cancelled, nothing stored")
            } else {
                e
            }
        })?
    } else {
        read_piped()?
    };
    let token = token.trim();
    if token.is_empty() {
        bail!("no token given: nothing stored");
    }
    match twr_core::runner::check_token_text(token, "the token") {
        Err(e) => bail!("{e}: nothing stored"),
        Ok(Some(warning)) => errln!("twr: {warning}"),
        Ok(None) => {}
    }
    let token = Secret::new(token);

    let path = &targets.token_file;
    let shown = path.display();
    // Asked only when the file holds another token, under the file's lock (two installs never
    // interleave): replaced only once confirmed (`--yes` without a terminal).
    let mut ask = super::ask_on_stderr;
    let mut replace = || -> anyhow::Result<bool> {
        let question = format!(
            "box {name} already has a token here ({shown}): is this the same hub's box {name} \
             (a new token), or another hub's? Two hubs need two box names. Replace it? [y/N] "
        );
        match super::consent(args.yes, &mut ask) {
            Consent::Yes => Ok(true),
            Consent::Ask(ask) => Ok(matches!(
                ask(&question)?.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            )),
            Consent::NoTerminal => bail!(
                "box {name} already has another token in {shown} (no terminal to ask on): pass \
                 --yes to replace it. Two hubs need two box names"
            ),
        }
    };
    let stored =
        twr_runner::client::write_token_file(path, &token, targets.default_file, &mut replace)?;
    match stored {
        Stored::Kept => bail!("nothing stored"),
        Stored::Same => outln!("box {name}'s token in {shown} is unchanged"),
        Stored::New | Stored::Replaced => {
            outln!("stored box {name}'s token in {shown} (mode 0600)");
            outln!("A runner of box {name} that is running takes it at its next connection.");
        }
    }

    let Some(hub) = &args.hub else {
        // The URL is a placeholder to fill in, not a shell word.
        let words = runner_args("URL", name, &targets).words;
        outln!(
            "Run it with `twr runner --hub <the hub's URL> {}`",
            run_command(&words[3..]).trim_start_matches("twr ")
        );
        return Ok(());
    };
    let words = runner_args(hub, name, &targets);
    outln!("Run the runner now:");
    outln!("  {}", run_command(&words.words));
    let service = match service_spec(name, &words, &env) {
        Ok(spec) => snippet(std::env::consts::OS, on_path("systemctl"), &spec),
        Err(note) => Service::Note(note),
    };
    match service {
        Service::Paste(text) => {
            outln!("Or keep it running (paste this):");
            out!("{text}");
        }
        Service::Note(note) => outln!("To keep it running, {note}."),
    }
    Ok(())
}

/// The token piped in: stdin to its end, at most 64 KiB, as text.
fn read_piped() -> anyhow::Result<String> {
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_PIPED + 1)
        .read_to_end(&mut buf)
        .context("reading the token from stdin")?;
    if buf.len() as u64 > MAX_PIPED {
        bail!("more than 64 KiB on stdin: that is not a token; nothing stored");
    }
    String::from_utf8(buf).map_err(|_| anyhow!("stdin is not text: nothing stored"))
}

/// Where `install-token` writes the token, and the paths the commands it prints name, all
/// absolute so a service doesn't depend on its working directory or environment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Targets {
    /// The token file: `--token-file` (a relative one under the working directory, its
    /// directory resolved), else the default one.
    token_file: PathBuf,
    /// It is the default file: its directory is made private when missing.
    default_file: bool,
    /// `--data-dir` for the runner, when its data directory isn't the standard one
    /// (`TWR_HOME` set, or `XDG_DATA_HOME` set elsewhere than `~/.local/share`): a service has
    /// neither variable.
    data_dir: Option<PathBuf>,
    /// `--ca`.
    ca: Option<PathBuf>,
}

/// [`Targets`] for box `name`, the environment read through `env`, relative paths taken from
/// `cwd`.
fn targets(
    name: &str,
    token_file: Option<&Path>,
    ca: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> anyhow::Result<Targets> {
    let paths = Paths::resolve_in(env, Some(DEFAULT_WORKSPACE))?;
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    let standard = var("TWR_HOME").is_none()
        && match (var("XDG_DATA_HOME"), var("HOME")) {
            (None, _) => true,
            (Some(data), Some(home)) => Path::new(&data) == Path::new(&home).join(".local/share"),
            (Some(_), None) => false,
        };
    let (token_file, default_file) = match token_file {
        Some(p) if p.is_relative() => {
            let joined = cwd.join(p);
            let (Some(dir), Some(file)) = (joined.parent(), joined.file_name()) else {
                bail!("--token-file {} names no file", p.display());
            };
            let dir = dir
                .canonicalize()
                .with_context(|| format!("the directory of --token-file {}", p.display()))?;
            (dir.join(file), false)
        }
        Some(p) => (p.to_path_buf(), false),
        None => (cwd.join(paths.runner_token_file(name)), true),
    };
    Ok(Targets {
        token_file,
        default_file,
        data_dir: (!standard).then(|| cwd.join(&paths.data_dir)),
        ca: ca.map(|c| cwd.join(c)),
    })
}

/// The arguments of the `twr runner` that uses the token `targets` names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerArgs {
    words: Vec<String>,
    /// Every path in them was UTF-8 (else a service is not written: its words would differ).
    exact: bool,
}

/// `runner --hub <hub> --box=<name> --token-file=<file> [--ca=<pem>] [--data-dir=<dir>]`.
fn runner_args(hub: &str, name: &str, targets: &Targets) -> RunnerArgs {
    let mut exact = true;
    let mut path_arg = |flag: &str, p: &Path| {
        exact &= p.to_str().is_some();
        format!("--{flag}={}", p.to_string_lossy())
    };
    let mut words = vec![
        "runner".to_string(),
        "--hub".to_string(),
        hub.to_string(),
        format!("--box={name}"),
        path_arg("token-file", &targets.token_file),
    ];
    if let Some(ca) = &targets.ca {
        words.push(path_arg("ca", ca));
    }
    if let Some(dir) = &targets.data_dir {
        words.push(path_arg("data-dir", dir));
    }
    RunnerArgs { words, exact }
}

/// `twr <words>`, each a shell word.
fn run_command(words: &[String]) -> String {
    let mut cmd = "twr".to_string();
    for w in words {
        cmd.push(' ');
        cmd.push_str(&sh_word(w));
    }
    cmd
}

/// Whether an executable file `name` is in a directory of `$PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        std::fs::metadata(d.join(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// What a service runs and with what: this executable, the runner's arguments, the
/// environment it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceSpec<'a> {
    box_name: &'a str,
    /// This executable, absolute.
    exe: String,
    args: &'a [String],
    /// `SHELL`, when it is an absolute path of plain characters: the runner asks the login
    /// shell for its `PATH` ([`twr_runner::BoxEnv::user`]).
    shell: Option<String>,
    /// `PATH`, for when the login shell can't say: this executable's directory, then the
    /// usual ones.
    path: String,
    /// The home directory (the Mac's log file).
    home: String,
    /// `XDG_CONFIG_HOME`, when set: the systemd user units' directory is under it.
    config_home: Option<String>,
}

/// The [`ServiceSpec`] of a runner with `args`, from this process and its environment
/// (`env`), or what to say when there is none.
fn service_spec<'a>(
    name: &'a str,
    args: &'a RunnerArgs,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ServiceSpec<'a>, String> {
    let by_hand = || {
        if std::env::consts::OS == "macos" {
            PLIST_BY_HAND.to_string()
        } else {
            UNIT_BY_HAND.to_string()
        }
    };
    if !args.exact {
        return Err(by_hand());
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .filter(|p| p.starts_with('/'))
        .ok_or_else(by_hand)?;
    let home = env("HOME")
        .filter(|h| h.starts_with('/'))
        .ok_or_else(by_hand)?;
    let shell = env("SHELL").filter(|s| {
        s.starts_with('/')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b))
    });
    let exe_dir = Path::new(&exe)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("/");
    let path = service_path(exe_dir, &home, std::env::consts::OS);
    Ok(ServiceSpec {
        box_name: name,
        exe,
        args: &args.words,
        shell,
        path,
        home,
        config_home: env("XDG_CONFIG_HOME").filter(|c| !c.is_empty()),
    })
}

/// A service's `PATH`: `exe_dir` (unless it holds a `:`), `~/.local/bin`, Homebrew's on a Mac,
/// then the system's; each once.
fn service_path(exe_dir: &str, home: &str, os: &str) -> String {
    let local = format!("{}/.local/bin", home.trim_end_matches('/'));
    let mut dirs: Vec<&str> = Vec::new();
    if !exe_dir.contains(':') {
        dirs.push(exe_dir);
    }
    dirs.push(&local);
    if os == "macos" {
        dirs.push("/opt/homebrew/bin");
    }
    dirs.extend(["/usr/local/bin", "/usr/bin", "/bin"]);
    let mut seen: Vec<&str> = Vec::new();
    for d in dirs {
        if !seen.contains(&d) {
            seen.push(d);
        }
    }
    seen.join(":")
}

/// What `install-token` prints to keep the runner running.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Service {
    /// Shell lines to paste: write the service, then start it.
    Paste(String),
    /// Why there is none, as the end of "To keep it running, ...".
    Note(String),
}

/// The service for `os` (`std::env::consts::OS`, passed in so both are tested anywhere): a
/// LaunchAgent on a Mac, a systemd user unit on Linux with `systemctl`; else a note.
fn snippet(os: &str, systemctl: bool, spec: &ServiceSpec) -> Service {
    match os {
        "macos" => match launch_agent(spec).and_then(|p| heredoc(&p, PLIST_END).then_some(p)) {
            Some(plist) => Service::Paste(format!(
                "# A LaunchAgent runs in your GUI session, where agents can read your login\n\
                 # keychain: it runs while you are logged in at the Mac's screen (logging out\n\
                 # stops it, logging in starts it again).\n\
                 mkdir -p ~/Library/LaunchAgents ~/Library/Logs\n\
                 cat > ~/Library/LaunchAgents/{label}.plist <<'{PLIST_END}'\n\
                 {plist}{PLIST_END}\n\
                 # Pasted again: the runner the last paste started stops first (and its agents\n\
                 # with it); none loaded is no error.\n\
                 launchctl bootout gui/$(id -u)/{label} 2>/dev/null || true\n\
                 launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{label}.plist\n",
                label = agent_label(spec.box_name),
            )),
            None => Service::Note(PLIST_BY_HAND.to_string()),
        },
        "linux" if systemctl => {
            match systemd_unit(spec).and_then(|u| heredoc(&u, UNIT_END).then_some(u)) {
                Some(unit) => Service::Paste(format!(
                    "mkdir -p {dir}\n\
                     cat > {dir}/{name} <<'{UNIT_END}'\n\
                     {unit}{UNIT_END}\n\
                     systemctl --user daemon-reload\n\
                     # Pasted again: a runner running already keeps its old settings until it\n\
                     # restarts, which stops its agents; twr leaves that to you.\n\
                     if systemctl --user is-active --quiet {name}; then echo \"{name} was running \
                     already: systemctl --user restart {name} to use the new settings (it stops \
                     the box's agents)\"; fi\n\
                     systemctl --user enable --now {name}\n\
                     loginctl enable-linger    # keeps it running when you are not logged in\n",
                    dir = unit_dir(spec.config_home.as_deref()),
                    name = unit_name(spec.box_name),
                )),
                None => Service::Note(UNIT_BY_HAND.to_string()),
            }
        }
        _ => Service::Note("run it under tmux, or as a service of your init system".to_string()),
    }
}

/// Whether `body` can go into a heredoc ended by `end`: no line of it is `end` (a backstop:
/// the values in it hold no newline, [`plain_values`]).
fn heredoc(body: &str, end: &str) -> bool {
    !body.lines().any(|l| l.trim() == end)
}

/// The systemd unit of box `name`'s runner, its whole file name: `twr-runner-<box>.service`
/// (given whole to `systemctl`, which adds no suffix to a name that ends in one already, as
/// box `lab.service`'s would).
pub fn unit_name(name: &str) -> String {
    format!("twr-runner-{name}.service")
}

/// Where the systemd user manager reads units the user writes, as a shell word:
/// `$XDG_CONFIG_HOME/systemd/user` (`config_home`, when absolute: quoted), else
/// `~/.config/systemd/user`.
fn unit_dir(config_home: Option<&str>) -> String {
    match config_home.filter(|c| c.starts_with('/')) {
        Some(c) => sh_word(&format!("{}/systemd/user", c.trim_end_matches('/'))),
        None => "~/.config/systemd/user".to_string(),
    }
}

/// The LaunchAgent's label of box `name`'s runner: `tools.twr.runner.<box>`, not the box
/// daemon's `tools.twr.box.` ([`launchd::LABEL_PREFIX`]), whose stale jobs a relay boots out.
pub fn agent_label(name: &str) -> String {
    format!("tools.twr.runner.{name}")
}

/// Whether no value of `spec` holds a control character (a newline above all: a value is
/// written into a heredoc, where a line could end it).
fn plain_values(spec: &ServiceSpec) -> bool {
    let plain = |s: &str| !s.chars().any(char::is_control);
    plain(&spec.exe)
        && spec.args.iter().all(|a| plain(a))
        && spec.shell.as_deref().is_none_or(plain)
        && plain(&spec.path)
        && plain(&spec.home)
        && spec.config_home.as_deref().is_none_or(plain)
}

/// `w` as one word of a systemd unit's `ExecStart=`: `%` as `%%` (a specifier otherwise), and
/// in an argument (not the executable, which systemd doesn't expand) `$` as `$$` (a variable
/// otherwise); in double quotes when it holds a space, `'` or `;`. `None` for a word with `"`,
/// `\` or a control character, or an empty one.
fn systemd_word(w: &str, executable: bool) -> Option<String> {
    if w.is_empty() || w.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return None;
    }
    let mut w = w.replace('%', "%%");
    if !executable {
        w = w.replace('$', "$$");
    }
    Some(if w.contains([' ', '\'', ';']) {
        format!("\"{w}\"")
    } else {
        w
    })
}

/// `name=value` as a systemd unit's `Environment=` value: `%` as `%%`, in double quotes when it
/// holds a space or `'` (`$` means nothing there). `None` for a value with `"`, `\` or a control
/// character.
fn systemd_env(name: &str, value: &str) -> Option<String> {
    if value
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        return None;
    }
    let a = format!("{name}={}", value.replace('%', "%%"));
    Some(if a.contains([' ', '\'']) {
        format!("\"{a}\"")
    } else {
        a
    })
}

/// The systemd user unit that keeps the runner of `spec` running: the unit of
/// docs/runners.md with its paths filled in. `None` when a value can't be written in it.
fn systemd_unit(spec: &ServiceSpec) -> Option<String> {
    if !plain_values(spec) {
        return None;
    }
    let mut exec = vec![systemd_word(&spec.exe, true)?];
    for a in spec.args {
        exec.push(systemd_word(a, false)?);
    }
    let mut env = String::new();
    if let Some(shell) = &spec.shell {
        env.push_str(&format!("Environment={}\n", systemd_env("SHELL", shell)?));
    }
    env.push_str(&format!(
        "Environment={}\n",
        systemd_env("PATH", &spec.path)?
    ));
    Some(format!(
        "[Unit]\n\
         Description=twr runner for box {name}\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         # A service reads no shell startup file: the runner asks your login shell (SHELL) for\n\
         # its PATH, and uses this one when it can't.\n\
         {env}\
         # Stop the runner first and let it stop its runtimes and finish a repository change (up\n\
         # to 5 minutes) before systemd kills what is left.\n\
         KillMode=mixed\n\
         TimeoutStopSec={stop}\n\
         Restart=always\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        name = spec.box_name,
        exec = exec.join(" "),
        stop = twr_runner::client::SHUTDOWN_BUDGET.as_secs(),
    ))
}

/// The LaunchAgent (a plist) that keeps the runner of `spec` running in the GUI session:
/// started at load and kept alive, `Interactive`, given the box daemon's exit timeout
/// ([`launchd::EXIT_TIMEOUT`]), a umask of 077 (its log private), its stderr (the runner's
/// log) in `~/Library/Logs/twr-runner-<box>.log`. Every string escaped ([`launchd::escape`]).
/// `None` when a value can't be written in it.
fn launch_agent(spec: &ServiceSpec) -> Option<String> {
    if !plain_values(spec) {
        return None;
    }
    let s = |v: &str| {
        launchd::representable(v).then(|| format!("<string>{}</string>", launchd::escape(v)))
    };
    let mut argv = format!("    {}\n", s(&spec.exe)?);
    for a in spec.args {
        argv.push_str(&format!("    {}\n", s(a)?));
    }
    let mut env = format!("    <key>PATH</key>{}\n", s(&spec.path)?);
    if let Some(shell) = &spec.shell {
        env.push_str(&format!("    <key>SHELL</key>{}\n", s(shell)?));
    }
    let log = format!(
        "{}/Library/Logs/twr-runner-{}.log",
        spec.home.trim_end_matches('/'),
        spec.box_name
    );
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>{label}\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {argv}\
         \x20 </array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict>\n\
         {env}\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>ProcessType</key><string>Interactive</string>\n\
         \x20 <key>ExitTimeOut</key><integer>{exit}</integer>\n\
         \x20 <key>Umask</key><integer>63</integer>\n\
         \x20 <key>StandardErrorPath</key>{log}\n\
         </dict>\n\
         </plist>\n",
        label = s(&agent_label(spec.box_name))?,
        exit = launchd::EXIT_TIMEOUT.as_secs(),
        log = s(&log)?,
    ))
}

/// The token `twr runner` uses, and the file it came from: `--token-file` (`given`), else
/// `TWR_RUNNER_TOKEN` (`env_token`, already taken out of the environment), else the default
/// file `default` when there is one (`twr runner install-token`), else an error that says how
/// to get one.
pub fn runner_token(
    given: Option<PathBuf>,
    env_token: Option<Secret>,
    default: PathBuf,
    box_name: &str,
) -> anyhow::Result<(Secret, Option<PathBuf>)> {
    if let Some(file) = given {
        return Ok((twr_runner::client::read_token_file(&file)?, Some(file)));
    }
    if let Some(token) = env_token {
        return Ok((token, None));
    }
    if std::fs::symlink_metadata(&default).is_ok() {
        return Ok((
            twr_runner::client::read_token_file(&default)?,
            Some(default),
        ));
    }
    bail!(
        "no runner token: run `twr runner install-token {box_name}`, or pass --token-file or set \
         {}",
        twr_runner::client::TOKEN_ENV
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// The paths the printed commands and services name are absolute, the token file's and
    /// the data directory's alike, whatever the environment that resolved them (R4).
    #[test]
    fn generated_commands_name_absolute_paths() {
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().canonicalize().unwrap();
        let t = |e: &dyn Fn(&str) -> Option<String>, file: Option<&Path>| {
            targets("gpu", file, None, e, &cwd).unwrap()
        };
        // The standard layout: the default file, no --data-dir.
        let std_env = env(&[("HOME", "/home/me")]);
        let got = t(&std_env, None);
        assert_eq!(
            got.token_file,
            Path::new("/home/me/.config/twr/runner-tokens/gpu.token")
        );
        assert!(got.default_file);
        assert_eq!(got.data_dir, None);
        let same = env(&[
            ("HOME", "/home/me"),
            ("XDG_DATA_HOME", "/home/me/.local/share/"),
        ]);
        assert_eq!(t(&same, None).data_dir, None);
        // XDG_CONFIG_HOME and XDG_DATA_HOME elsewhere: both named.
        let xdg = env(&[
            ("HOME", "/home/me"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/srv/data"),
        ]);
        let got = t(&xdg, None);
        assert_eq!(
            got.token_file,
            Path::new("/cfg/twr/runner-tokens/gpu.token")
        );
        assert_eq!(got.data_dir.as_deref(), Some(Path::new("/srv/data/twr")));
        let words = runner_args("wss://hub:7878", "gpu", &got).words;
        assert_eq!(
            words,
            [
                "runner",
                "--hub",
                "wss://hub:7878",
                "--box=gpu",
                "--token-file=/cfg/twr/runner-tokens/gpu.token",
                "--data-dir=/srv/data/twr",
            ]
        );
        // A relative TWR_HOME: absolute from the working directory.
        let home = env(&[("HOME", "/home/me"), ("TWR_HOME", "box-home")]);
        let got = t(&home, None);
        assert_eq!(got.token_file, cwd.join("box-home/runner-tokens/gpu.token"));
        assert_eq!(got.data_dir, Some(cwd.join("box-home/data")));
        // A relative --token-file: its directory resolved (through a symbolic link too).
        std::fs::create_dir(cwd.join("real")).unwrap();
        std::os::unix::fs::symlink(cwd.join("real"), cwd.join("link")).unwrap();
        let got = t(&std_env, Some(Path::new("link/../real/./gpu.token")));
        assert_eq!(got.token_file, cwd.join("real/gpu.token"));
        assert!(!got.default_file);
        let got = t(&std_env, Some(Path::new("gpu.token")));
        assert_eq!(got.token_file, cwd.join("gpu.token"));
        assert!(targets(
            "gpu",
            Some(Path::new("missing/gpu.token")),
            None,
            &std_env,
            &cwd
        )
        .is_err());
        let got = targets("gpu", None, Some(Path::new("ca.pem")), &std_env, &cwd).unwrap();
        assert_eq!(got.ca, Some(cwd.join("ca.pem")));
        for t in [got] {
            let words = runner_args("ws://127.0.0.1:7878", "gpu", &t).words;
            for w in &words[4..] {
                let (_, p) = w.split_once('=').unwrap();
                assert!(Path::new(p).is_absolute(), "{w}");
            }
        }
    }

    fn spec<'a>(args: &'a [String]) -> ServiceSpec<'a> {
        ServiceSpec {
            box_name: "gpu",
            exe: "/home/me/.local/bin/twr".into(),
            args,
            shell: Some("/bin/bash".into()),
            path: service_path("/home/me/.local/bin", "/home/me", "linux"),
            home: "/home/me".into(),
            config_home: None,
        }
    }

    fn args() -> Vec<String> {
        runner_args(
            "wss://hub.tail1234.ts.net:7878",
            "gpu",
            &Targets {
                token_file: "/home/me/.config/twr/runner-tokens/gpu.token".into(),
                default_file: true,
                data_dir: None,
                ca: None,
            },
        )
        .words
    }

    #[test]
    fn the_systemd_unit_is_docs_unit_filled_in() {
        let args = args();
        let Service::Paste(text) = snippet("linux", true, &spec(&args)) else {
            panic!("no unit")
        };
        assert_eq!(
            text,
            "mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/twr-runner-gpu.service <<'TWR_UNIT'
[Unit]
Description=twr runner for box gpu

[Service]
ExecStart=/home/me/.local/bin/twr runner --hub wss://hub.tail1234.ts.net:7878 --box=gpu --token-file=/home/me/.config/twr/runner-tokens/gpu.token
# A service reads no shell startup file: the runner asks your login shell (SHELL) for
# its PATH, and uses this one when it can't.
Environment=SHELL=/bin/bash
Environment=PATH=/home/me/.local/bin:/usr/local/bin:/usr/bin:/bin
# Stop the runner first and let it stop its runtimes and finish a repository change (up
# to 5 minutes) before systemd kills what is left.
KillMode=mixed
TimeoutStopSec=330
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
TWR_UNIT
systemctl --user daemon-reload
# Pasted again: a runner running already keeps its old settings until it
# restarts, which stops its agents; twr leaves that to you.
if systemctl --user is-active --quiet twr-runner-gpu.service; then echo \"twr-runner-gpu.service was running already: systemctl --user restart twr-runner-gpu.service to use the new settings (it stops the box's agents)\"; fi
systemctl --user enable --now twr-runner-gpu.service
loginctl enable-linger    # keeps it running when you are not logged in
"
        );
        // Under XDG_CONFIG_HOME, where the user manager reads units: its absolute path,
        // quoted.
        let mut s = spec(&args);
        s.config_home = Some("/srv/my cfg/".into());
        let Service::Paste(text) = snippet("linux", true, &s) else {
            panic!("no unit")
        };
        assert!(
            text.starts_with(
                "mkdir -p '/srv/my cfg/systemd/user'\n\
                 cat > '/srv/my cfg/systemd/user'/twr-runner-gpu.service <<'TWR_UNIT'\n"
            ),
            "{text}"
        );
        s.config_home = Some("relative".into());
        let Service::Paste(text) = snippet("linux", true, &s) else {
            panic!("no unit")
        };
        assert!(
            text.starts_with("mkdir -p ~/.config/systemd/user\n"),
            "{text}"
        );
        // A box name that ends in a unit suffix: the whole file name, to systemctl too.
        for name in ["lab.service", "lab.timer"] {
            let mut s = spec(&args);
            s.box_name = name;
            let Service::Paste(text) = snippet("linux", true, &s) else {
                panic!("no unit")
            };
            let unit = format!("twr-runner-{name}.service");
            assert!(
                text.contains(&format!("/systemd/user/{unit} <<'TWR_UNIT'\n"))
                    && text.contains(&format!("systemctl --user enable --now {unit}\n"))
                    && text.contains(&format!("is-active --quiet {unit}; then"))
                    && text.contains(&format!("systemctl --user restart {unit} to use")),
                "{text}"
            );
        }
        // Without systemctl, or on another system: a note, no snippet.
        assert!(
            matches!(snippet("linux", false, &spec(&args)), Service::Note(n) if n.contains("tmux"))
        );
        assert!(matches!(
            snippet("freebsd", true, &spec(&args)),
            Service::Note(_)
        ));
    }

    #[test]
    fn unit_words_are_escaped_for_systemd() {
        let arg = |w: &str| systemd_word(w, false);
        assert_eq!(arg("/opt/twr").as_deref(), Some("/opt/twr"));
        assert_eq!(arg("/opt/100%/twr").as_deref(), Some("/opt/100%%/twr"));
        assert_eq!(
            arg("--hub=wss://h/$x").as_deref(),
            Some("--hub=wss://h/$$x")
        );
        // systemd expands no variable in the executable's path.
        assert_eq!(
            systemd_word("/opt/$HOME/100%/twr", true).as_deref(),
            Some("/opt/$HOME/100%%/twr")
        );
        assert_eq!(
            arg("/opt/my twr/twr").as_deref(),
            Some("\"/opt/my twr/twr\"")
        );
        assert_eq!(arg("/opt/it's/twr").as_deref(), Some("\"/opt/it's/twr\""));
        assert_eq!(arg(";").as_deref(), Some("\";\""));
        for bad in ["", "/opt/\"/twr", "/opt/a\\b", "/opt/a\tb"] {
            assert_eq!(arg(bad), None, "{bad:?}");
            assert_eq!(systemd_word(bad, true), None, "{bad:?}");
        }
        assert_eq!(
            systemd_env("PATH", "/my bin:/usr/bin").as_deref(),
            Some("\"PATH=/my bin:/usr/bin\"")
        );
        assert_eq!(
            systemd_env("PATH", "/a%b:/$x").as_deref(),
            Some("PATH=/a%%b:/$x")
        );
        assert_eq!(systemd_env("PATH", "/a\"b"), None);
        // A word that can't be written: the unit is not printed.
        let args = args();
        let mut s = spec(&args);
        s.exe = "/opt/\"odd\"/twr".into();
        assert_eq!(
            snippet("linux", true, &s),
            Service::Note(UNIT_BY_HAND.into())
        );
    }

    /// The unit as systemd reads it: `systemd-analyze verify` passes it (where the tool is),
    /// with the executable's odd characters escaped.
    #[test]
    fn systemd_accepts_the_unit() {
        let Ok(out) = std::process::Command::new("timeout")
            .args(["30", "systemd-analyze", "--version"])
            .output()
        else {
            eprintln!("no systemd-analyze here: skipped");
            return;
        };
        if !out.status.success() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("100% my $twr");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("twr");
        std::fs::copy("/bin/true", &exe).unwrap();
        let args = args();
        let mut s = spec(&args);
        s.exe = exe.to_str().unwrap().to_string();
        let unit = systemd_unit(&s).unwrap();
        assert!(
            unit.contains("\"/") && unit.contains("/100%% my $twr/twr\" runner"),
            "{unit}"
        );
        let file = dir.path().join("twr-runner-gpu.service");
        std::fs::write(&file, &unit).unwrap();
        let out = std::process::Command::new("timeout")
            .args(["30", "systemd-analyze", "--user", "verify"])
            .arg(&file)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{unit}\n{err}");
        assert!(!err.contains("twr-runner-gpu.service"), "{err}");
    }

    #[test]
    fn the_launch_agent_is_a_plist_with_escaped_strings() {
        let args = args();
        let mut s = spec(&args);
        s.exe = "/Users/me/.local/bin/twr".into();
        s.home = "/Users/me".into();
        s.shell = Some("/bin/zsh".into());
        s.path = service_path("/Users/me/.local/bin", "/Users/me", "macos");
        let Service::Paste(text) = snippet("macos", false, &s) else {
            panic!("no plist")
        };
        assert_eq!(
            text,
            "# A LaunchAgent runs in your GUI session, where agents can read your login
# keychain: it runs while you are logged in at the Mac's screen (logging out
# stops it, logging in starts it again).
mkdir -p ~/Library/LaunchAgents ~/Library/Logs
cat > ~/Library/LaunchAgents/tools.twr.runner.gpu.plist <<'TWR_PLIST'
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key><string>tools.twr.runner.gpu</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/.local/bin/twr</string>
    <string>runner</string>
    <string>--hub</string>
    <string>wss://hub.tail1234.ts.net:7878</string>
    <string>--box=gpu</string>
    <string>--token-file=/home/me/.config/twr/runner-tokens/gpu.token</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/Users/me/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>SHELL</key><string>/bin/zsh</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>ExitTimeOut</key><integer>330</integer>
  <key>Umask</key><integer>63</integer>
  <key>StandardErrorPath</key><string>/Users/me/Library/Logs/twr-runner-gpu.log</string>
</dict>
</plist>
TWR_PLIST
# Pasted again: the runner the last paste started stops first (and its agents
# with it); none loaded is no error.
launchctl bootout gui/$(id -u)/tools.twr.runner.gpu 2>/dev/null || true
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tools.twr.runner.gpu.plist
"
        );
        // `&` and `<` in a home path are escaped; the label is not the box daemon's.
        s.home = "/Users/a&b<c".into();
        let plist = launch_agent(&s).unwrap();
        assert!(
            plist.contains("<string>/Users/a&amp;b&lt;c/Library/Logs/twr-runner-gpu.log</string>"),
            "{plist}"
        );
        assert!(!agent_label("gpu").starts_with(launchd::LABEL_PREFIX));
        // A value XML can't hold: no plist.
        s.home = "/Users/\u{fffe}".into();
        assert_eq!(launch_agent(&s), None);
        assert_eq!(
            snippet("macos", false, &s),
            Service::Note(PLIST_BY_HAND.into())
        );
    }

    /// A value with a newline could end the heredoc early and run what follows as shell
    /// lines: no snippet is printed for it, whichever value it is in (R7).
    #[test]
    fn no_value_can_end_the_heredoc() {
        let args = args();
        let injected = [
            "/opt/x\nTWR_UNIT\ntouch /tmp/pwned\n/twr",
            "/opt/x\nTWR_PLIST\ntouch /tmp/pwned\n/twr",
            "/opt/x\r/twr",
            "/opt/x\u{85}/twr",
        ];
        for bad in injected {
            for os in ["linux", "macos"] {
                let mut s = spec(&args);
                s.exe = bad.into();
                assert!(
                    matches!(snippet(os, true, &s), Service::Note(_)),
                    "{os} {bad:?}"
                );
                let mut s = spec(&args);
                s.home = bad.into();
                assert!(
                    matches!(snippet(os, true, &s), Service::Note(_)),
                    "{os} {bad:?}"
                );
                let mut s = spec(&args);
                s.path = bad.into();
                assert!(
                    matches!(snippet(os, true, &s), Service::Note(_)),
                    "{os} {bad:?}"
                );
                let mut s = spec(&args);
                s.shell = Some(bad.into());
                assert!(
                    matches!(snippet(os, true, &s), Service::Note(_)),
                    "{os} {bad:?}"
                );
                let mut odd = args.clone();
                odd[2] = bad.into();
                let s = spec(&odd);
                assert!(
                    matches!(snippet(os, true, &s), Service::Note(_)),
                    "{os} {bad:?}"
                );
            }
        }
        // The backstop on its own: a line that is the delimiter.
        assert!(!heredoc("a\nTWR_UNIT\nb", UNIT_END));
        assert!(!heredoc("a\n TWR_UNIT \nb", UNIT_END));
        assert!(heredoc("a/TWR_UNIT/b\n", UNIT_END));
    }

    /// `twr runner`'s token: --token-file, else the variable, else the default file, else an
    /// error naming install-token.
    #[test]
    fn the_runner_takes_its_token_in_order() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, token: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, token).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            p
        };
        let given = write("given.token", "given-token-0123456789");
        let default = write("default.token", "default-token-0123456789");
        let env = || Some(Secret::new("env-token-0123456789"));
        let (t, f) = runner_token(Some(given.clone()), env(), default.clone(), "gpu").unwrap();
        assert_eq!((t.expose(), f), ("given-token-0123456789", Some(given)));
        let (t, f) = runner_token(None, env(), default.clone(), "gpu").unwrap();
        assert_eq!((t.expose(), f), ("env-token-0123456789", None));
        let (t, f) = runner_token(None, None, default.clone(), "gpu").unwrap();
        assert_eq!(
            (t.expose(), f),
            ("default-token-0123456789", Some(default.clone()))
        );
        // A default file the runner can't use is an error, not skipped.
        std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = runner_token(None, None, default, "gpu").unwrap_err();
        assert!(e.to_string().contains("chmod 600"), "{e:#}");
        let e = runner_token(None, None, dir.path().join("none.token"), "gpu").unwrap_err();
        assert_eq!(
            e.to_string(),
            "no runner token: run `twr runner install-token gpu`, or pass --token-file or set \
             TWR_RUNNER_TOKEN"
        );
    }
}

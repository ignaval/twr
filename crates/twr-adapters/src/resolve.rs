//! Where an agent runtime's program is, and the `PATH` it runs with: one resolution for the
//! spawn, the resume, Claude Code's capability probe and the box's agent probe, so each of
//! them looks at the binary the runtime runs.
//!
//! # The login shell's `PATH`
//!
//! A runner started by ssh, systemd or cron has a thin `PATH`: no `~/.local/bin`, no nvm or
//! npm prefix, and many `.bashrc` files return early when the shell is not interactive. A
//! runner (token or daemon) therefore asks the user's login shell for its `PATH` once, in the
//! background as it starts ([`ask_login_path`]): `$SHELL -lic '<print>'` for sh, dash, bash,
//! zsh, ksh and mksh (interactive too, so rc files behind an interactive guard run), fish
//! with its own syntax (`string join : $PATH`); csh, tcsh and any other shell are not asked
//! ([`LoginShell`]). The shell runs in a session of its own (no controlling terminal), stdin
//! `/dev/null`, stderr discarded, for at most [`LOGIN_PATH_TIMEOUT`], reading at most
//! [`LOGIN_PATH_MAX_OUTPUT`] bytes. It prints the `PATH` between two markers made with a
//! fresh random nonce (`__TWR_PATH_<nonce>__`; the command line never holds them whole, so an
//! rc file that echoes it is no match), and only the value between them is used: banners and
//! anything else an rc file prints are ignored. A value with a control character is refused;
//! of the others only absolute entries are kept, each once, at most
//! [`LOGIN_PATH_MAX_ENTRIES`] ([`LoginPath::parse`]). A shell that fails, times out or prints
//! no such value leaves the runtimes on the runner's own `PATH`, and the runner says why
//! once.
//!
//! The process environment is never changed ([`std::env::set_var`] in a multithreaded
//! process is unsound): the `PATH` goes into each runtime's environment instead
//! ([`with_login_path`]).
//!
//! # The resolution
//!
//! The `PATH` a runtime gets ([`runtime_path`]) is its thread's (`[threads] env` /
//! `[projects.<name>] env`, which wins as it always has), else, where a login `PATH` was
//! resolved, the login shell's entries followed by the runner's own `PATH` (duplicates
//! dropped), else nothing (it inherits the runner's, as every runtime of a hub's local box
//! does: the hub resolves no login `PATH`). Its program ([`resolve`]) is `TWR_*_BIN` (or the
//! adapter's own binary) when set, else the runtime's command (`claude`, `codex`, `grok`),
//! looked up on that `PATH` when it is a bare name.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, Bounded};

/// How long the login shell may take to print its `PATH`.
pub const LOGIN_PATH_TIMEOUT: Duration = Duration::from_secs(5);

/// The most the login shell may print (banners included) before its `PATH` has come.
pub const LOGIN_PATH_MAX_OUTPUT: usize = 16 << 10;

/// The most entries kept of the login shell's `PATH`.
pub const LOGIN_PATH_MAX_ENTRIES: usize = 128;

/// The built-in agent runtimes with a CLI of their own: the runtime, its command and the
/// environment variable naming another binary.
pub const BUILTIN_AGENTS: &[(&str, &str, &str)] = &[
    (
        crate::claude_code::NAME,
        "claude",
        crate::claude_code::BIN_ENV,
    ),
    (crate::codex::NAME, "codex", crate::codex::BIN_ENV),
    (crate::grok_build::NAME, "grok", crate::grok_build::BIN_ENV),
];

/// How the user's login shell is asked for its `PATH` (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginShell {
    /// sh, dash, bash, zsh, ksh or mksh: `-lic` with `printf`.
    Posix(PathBuf),
    /// fish: `-lic` with `string join`.
    Fish(PathBuf),
    /// A shell twr does not ask (csh and tcsh, whose syntax differs, or one it does not
    /// know).
    Unsupported(PathBuf),
    /// `SHELL` is not set.
    Unset,
}

impl LoginShell {
    /// The login shell `$SHELL` names.
    pub fn from_env() -> LoginShell {
        LoginShell::of(std::env::var_os("SHELL").as_deref())
    }

    /// The login shell `shell` (a `SHELL` value) names, by its file name.
    pub fn of(shell: Option<&OsStr>) -> LoginShell {
        let Some(shell) = shell.filter(|s| !s.is_empty()) else {
            return LoginShell::Unset;
        };
        let path = PathBuf::from(shell);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match name.as_str() {
            "sh" | "dash" | "bash" | "zsh" | "ksh" | "mksh" => LoginShell::Posix(path),
            "fish" => LoginShell::Fish(path),
            _ => LoginShell::Unsupported(path),
        }
    }

    /// The shell's path, when `SHELL` is set.
    pub fn path(&self) -> Option<&Path> {
        match self {
            LoginShell::Posix(p) | LoginShell::Fish(p) | LoginShell::Unsupported(p) => Some(p),
            LoginShell::Unset => None,
        }
    }
}

/// The login shell's `PATH` entries: absolute, each once, at most
/// [`LOGIN_PATH_MAX_ENTRIES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginPath {
    entries: Vec<PathBuf>,
}

impl LoginPath {
    /// The `PATH` value `value` (what the login shell printed between the markers): refused
    /// when it is not UTF-8 or holds a control character (a newline included), or keeps no
    /// entry; otherwise its absolute entries, each once, the first
    /// [`LOGIN_PATH_MAX_ENTRIES`] of them.
    pub fn parse(value: &[u8]) -> Result<LoginPath, String> {
        let value = std::str::from_utf8(value).map_err(|_| "its PATH is not UTF-8".to_string())?;
        if value.chars().any(char::is_control) {
            return Err("its PATH holds a control character".into());
        }
        let mut entries: Vec<PathBuf> = Vec::new();
        for e in value.split(':').map(PathBuf::from) {
            if e.is_absolute() && !entries.contains(&e) {
                entries.push(e);
            }
        }
        if entries.len() > LOGIN_PATH_MAX_ENTRIES {
            tracing::warn!(
                entries = entries.len(),
                kept = LOGIN_PATH_MAX_ENTRIES,
                "the login shell's PATH is long: keeping its first entries"
            );
            entries.truncate(LOGIN_PATH_MAX_ENTRIES);
        }
        if entries.is_empty() {
            return Err("its PATH has no absolute entry".into());
        }
        Ok(LoginPath { entries })
    }

    /// Its entries, in order.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// The `PATH` a runtime gets: these entries, then those of `runner` (the runner's own
    /// `PATH`) not already among them.
    pub fn merged(&self, runner: Option<&OsStr>) -> OsString {
        let mut all: Vec<PathBuf> = self.entries.clone();
        for e in runner.map(std::env::split_paths).into_iter().flatten() {
            if !all.contains(&e) {
                all.push(e);
            }
        }
        // Only absolute entries without `:` were kept, and the runner's were split on it.
        std::env::join_paths(all).unwrap_or_default()
    }
}

/// The marker around the `PATH` the login shell prints, for `nonce`.
fn marker(nonce: &str) -> String {
    format!("__TWR_PATH_{nonce}__")
}

/// The value between the first two `nonce` markers of `out`, once both are there.
fn framed<'a>(out: &'a [u8], nonce: &str) -> Option<&'a [u8]> {
    let m = marker(nonce);
    let m = m.as_bytes();
    let find = |hay: &[u8]| hay.windows(m.len()).position(|w| w == m);
    let start = find(out)? + m.len();
    let len = find(&out[start..])?;
    Some(&out[start..start + len])
}

/// Asks `shell` for its `PATH` (see the module docs), within `limit`: the entries, or why
/// there are none (a short phrase, never what the shell printed).
pub async fn ask_login_path(shell: &LoginShell, limit: Duration) -> Result<LoginPath, String> {
    let (program, script) = match shell {
        LoginShell::Unset => return Err("SHELL is not set".into()),
        LoginShell::Unsupported(p) => {
            return Err(format!(
                "{} is not asked (twr asks sh, bash, zsh, ksh and fish)",
                p.display()
            ))
        }
        // The format holds the marker split, so a shell that echoes its command prints no
        // marker.
        LoginShell::Posix(p) => (
            p,
            "printf '__TWR_PATH_%s__%s__TWR_PATH_%s__' \"$1\" \"$PATH\" \"$1\"",
        ),
        LoginShell::Fish(p) => (
            p,
            "printf '__TWR_PATH_%s__%s__TWR_PATH_%s__' $argv[1] (string join : $PATH) $argv[1]",
        ),
    };
    let nonce = twr_core::runner::new_nonce()
        .map_err(|e| format!("no random marker: {e}"))?
        .chars()
        .take(16)
        .collect::<String>();
    let mut cmd = tokio::process::Command::new(program);
    match shell {
        // `sh -c script name arg`: `$0` is `name`, `$1` the nonce.
        LoginShell::Posix(_) => cmd.args(["-lic", script, "twr", &nonce]),
        // fish: the arguments after the script are `$argv`.
        _ => cmd.args(["-lic", script, &nonce]),
    };
    let out = process::run_bounded(cmd, limit, LOGIN_PATH_MAX_OUTPUT, |o| {
        framed(o, &nonce).is_some()
    })
    .await;
    let printed = match out {
        Bounded::Enough(o) | Bounded::Exited(_, o) => o,
        Bounded::NotStarted => return Err(format!("{} could not be run", program.display())),
        Bounded::TooLarge => {
            return Err(format!(
                "it printed more than {} KiB before its PATH",
                LOGIN_PATH_MAX_OUTPUT >> 10
            ))
        }
        Bounded::TimedOut => return Err(format!("it did not answer within {limit:?}")),
        Bounded::Failed => return Err("reading its output failed".into()),
    };
    let value = framed(&printed, &nonce).ok_or("it printed no PATH")?;
    LoginPath::parse(value)
}

/// The `PATH` a runtime started with `env` added gets (see the module docs): `env`'s own
/// when it sets one, else `login` merged before the runner's `PATH` (`runner`), else `None`
/// (it inherits the runner's).
pub fn runtime_path_with(
    env: &[(String, String)],
    login: Option<&LoginPath>,
    runner: Option<&OsStr>,
) -> Option<OsString> {
    match process::env_path(env) {
        Some(p) => Some(p.into()),
        None => login.map(|l| l.merged(runner)),
    }
}

/// [`runtime_path_with`] with this process's `PATH` as the runner's.
pub fn runtime_path(env: &[(String, String)], login: Option<&LoginPath>) -> Option<OsString> {
    runtime_path_with(env, login, std::env::var_os("PATH").as_deref())
}

/// Adds the `PATH` a runtime gets ([`runtime_path`]) to its environment `env`, when `env`
/// sets none and a login `PATH` was resolved: what a runner does to every spawn and resume
/// before the adapter resolves the program from `env` ([`resolve`]). Nothing otherwise (a
/// hub's local box: `login` is `None`).
pub fn with_login_path(env: &mut Vec<(String, String)>, login: Option<&LoginPath>) {
    if process::env_path(env).is_some() {
        return;
    }
    if let Some(p) = runtime_path(env, login) {
        env.push(("PATH".into(), p.to_string_lossy().into_owned()));
    }
}

/// The program built-in agent runtime `runtime` runs ([`BUILTIN_AGENTS`]), the one choice every
/// path makes (the adapters' `bin()`, the agent probe, `twr runner --login` and `--exec`):
/// `explicit` (an adapter made `with_bin`), else its `TWR_*_BIN` as `var` reads it (the process
/// environment's), else its command (`claude`, `codex`, `grok`). An empty value counts as
/// unset, wherever it comes from (`TWR_CODEX_BIN=` runs `codex` from `PATH`, as an unset one
/// does). `None` for another runtime.
pub fn select_program(
    runtime: &str,
    explicit: Option<&Path>,
    var: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let (_, command, bin_env) = BUILTIN_AGENTS.iter().find(|(r, ..)| *r == runtime)?;
    Some(
        explicit
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| var(bin_env).filter(|v| !v.is_empty()).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(command)),
    )
}

/// [`select_program`] without an adapter's own binary.
pub fn runtime_program(runtime: &str, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    select_program(runtime, None, var)
}

/// A runtime's program as the runtime will be started ([`resolve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The program as named: `TWR_*_BIN` (or the adapter's own binary), else the runtime's
    /// command. What the runtime's process is started as.
    pub program: PathBuf,
    /// The executable file it names: a bare name looked up on [`Resolved::path`] (else the
    /// runner's `PATH`), a relative path taken from the working directory. `None`: there is
    /// none (the start fails with "`claude` not found on PATH").
    pub found: Option<PathBuf>,
    /// The `PATH` the runtime gets ([`runtime_path`]); `None`: the runner's.
    pub path: Option<OsString>,
}

/// Resolves `program` (`TWR_*_BIN` or the adapter's binary, else the runtime's command) for
/// a runtime started in `cwd` with `env` added: the `PATH` it gets ([`runtime_path`] of `env`
/// and `login`) and the executable file found on it. Adapters pass `login` `None`: a runner
/// has already put the login `PATH` into `env` ([`with_login_path`]). The box's agent probe
/// and `twr runner --login` pass an empty `env` and the login `PATH`: the environment of a
/// thread that configures no `PATH`.
pub fn resolve(
    program: &Path,
    env: &[(String, String)],
    login: Option<&LoginPath>,
    cwd: &Path,
) -> Resolved {
    let path = runtime_path(env, login);
    let search = path.clone().or_else(|| std::env::var_os("PATH"));
    let base = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        cwd.to_path_buf()
    };
    Resolved {
        found: process::find_program(program, search.as_deref(), &base),
        program: program.to_path_buf(),
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Creates the executable `path` with `content` without this process holding it open for
    /// writing (`cp` writes it: a fork holding it would make exec'ing it fail with ETXTBSY).
    fn install(path: &Path, content: &str) {
        let src = path.with_extension("src");
        std::fs::write(&src, content).unwrap();
        let st = std::process::Command::new("cp")
            .arg(&src)
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn shells_by_name() {
        let of = |s: &str| LoginShell::of(Some(OsStr::new(s)));
        for s in [
            "/bin/sh",
            "/bin/bash",
            "/usr/bin/zsh",
            "/bin/ksh",
            "dash",
            "/bin/mksh",
        ] {
            assert!(matches!(of(s), LoginShell::Posix(_)), "{s}");
        }
        assert_eq!(
            of("/usr/bin/fish"),
            LoginShell::Fish("/usr/bin/fish".into())
        );
        for s in ["/bin/csh", "/bin/tcsh", "/usr/bin/nu", "/usr/bin/bash5"] {
            assert!(matches!(of(s), LoginShell::Unsupported(_)), "{s}");
        }
        assert_eq!(LoginShell::of(None), LoginShell::Unset);
        assert_eq!(LoginShell::of(Some(OsStr::new(""))), LoginShell::Unset);
        assert_eq!(of("/bin/tcsh").path(), Some(Path::new("/bin/tcsh")));
        assert_eq!(LoginShell::Unset.path(), None);
    }

    /// Only absolute entries, each once, at most 128; a control character (or a value that is
    /// not UTF-8, or keeps nothing) refuses the whole value.
    #[test]
    fn login_path_values() {
        let p = LoginPath::parse(b"/a/bin::rel/bin:/b:/a/bin:.:~/x:/c").unwrap();
        let want: Vec<PathBuf> = ["/a/bin", "/b", "/c"].iter().map(PathBuf::from).collect();
        assert_eq!(p.entries(), want);
        for bad in [
            &b"/a\n/b"[..],
            b"/a:/b\x1b[0m",
            b"/a\t",
            b"/a\xff",
            b"",
            b"rel:./x:",
        ] {
            assert!(LoginPath::parse(bad).is_err(), "{bad:?}");
        }
        let long: Vec<String> = (0..200).map(|i| format!("/d{i}")).collect();
        let p = LoginPath::parse(long.join(":").as_bytes()).unwrap();
        assert_eq!(p.entries().len(), LOGIN_PATH_MAX_ENTRIES);
        assert_eq!(p.entries()[0], Path::new("/d0"));
        // Merged before the runner's own, duplicates dropped.
        let p = LoginPath::parse(b"/home/u/.local/bin:/usr/bin").unwrap();
        assert_eq!(
            p.merged(Some(OsStr::new("/usr/bin:/bin:/home/u/.local/bin"))),
            "/home/u/.local/bin:/usr/bin:/bin"
        );
        assert_eq!(p.merged(None), "/home/u/.local/bin:/usr/bin");
    }

    #[test]
    fn framing() {
        let n = "0123abcd";
        let out = b"Welcome!\n__TWR_PATH_0123abcd__/a:/b__TWR_PATH_0123abcd__\nbye";
        assert_eq!(framed(out, n), Some(&b"/a:/b"[..]));
        assert_eq!(framed(b"__TWR_PATH_0123abcd__/a:/b", n), None);
        assert_eq!(framed(b"__TWR_PATH_other__/a__TWR_PATH_other__", n), None);
        // The echoed command line holds no whole marker.
        assert_eq!(
            framed(
                b"printf '__TWR_PATH_%s__%s__TWR_PATH_%s__' \"$1\" \"$PATH\" \"$1\"",
                n
            ),
            None
        );
    }

    /// A shell `shell.sh` named `name` in `dir` (the name picks how it is asked).
    fn shell(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        install(&p, &format!("#!/bin/sh\n{body}\n"));
        p
    }

    /// A noisy shell: banners before and after, then the PATH the command prints; the
    /// framed value is what counts.
    #[tokio::test]
    async fn a_noisy_shell_gives_its_framed_path() {
        let dir = tempfile::tempdir().unwrap();
        // A fake `bash` that prints a banner, sets a PATH and runs the command as sh would
        // (`-lic script name nonce`).
        let sh = shell(
            dir.path(),
            "bash",
            "echo 'Welcome to the box'; echo __TWR_PATH_bogus__/x__TWR_PATH_bogus__\n\
             PATH=/opt/agents/bin:/usr/bin:relative; export PATH\n\
             shift; script=$1; shift; /bin/sh -c \"$script\" \"$@\"; echo; echo 'Goodbye'",
        );
        let got = ask_login_path(&LoginShell::Posix(sh), LOGIN_PATH_TIMEOUT)
            .await
            .unwrap();
        let want: Vec<PathBuf> = ["/opt/agents/bin", "/usr/bin"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(got.entries(), want);
    }

    /// A real bash whose `.bashrc` returns early when not interactive: the interactive login
    /// call still gets the PATH it sets after the guard.
    #[tokio::test]
    async fn bash_rc_files_behind_an_interactive_guard_run() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            home.join(".bash_profile"),
            "echo profile banner\n. ~/.bashrc\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "case $- in *i*) ;; *) return;; esac\necho rc banner\nexport PATH=\"/opt/after-guard/bin:$PATH\"\n",
        )
        .unwrap();
        let sh = shell(
            dir.path(),
            "bash",
            &format!("HOME='{}' exec /bin/bash \"$@\"", home.display()),
        );
        let got = ask_login_path(&LoginShell::Posix(sh), LOGIN_PATH_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(got.entries()[0], Path::new("/opt/after-guard/bin"));
    }

    /// The shell is done with once its framed PATH is read: killed and reaped before
    /// `ask_login_path` returns, so `twr runner --exec` / `--login`, which `exec` next, leave
    /// the agent no zombie child.
    #[tokio::test]
    async fn the_asked_shell_is_reaped_before_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let sh = shell(
            dir.path(),
            "sh",
            &format!(
                "echo $$ > '{}'\nprintf '__TWR_PATH_%s__/opt/bin__TWR_PATH_%s__' \"$4\" \"$4\"\nexec sleep 30",
                pidfile.display()
            ),
        );
        let got = ask_login_path(&LoginShell::Posix(sh), LOGIN_PATH_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(got.entries()[0], Path::new("/opt/bin"));
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        assert!(
            stat.trim().is_empty(),
            "the shell {pid} is still there: {stat}"
        );
    }

    /// fish is asked in its own syntax: the arguments after the script are `$argv`.
    #[tokio::test]
    async fn fish_is_asked_in_its_syntax() {
        let dir = tempfile::tempdir().unwrap();
        // Records its arguments, then answers as fish would for that script.
        let args = dir.path().join("args");
        let sh = shell(
            dir.path(),
            "fish",
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\nprintf '__TWR_PATH_%s__/fish/bin:/usr/bin__TWR_PATH_%s__' \"$3\" \"$3\"",
                args.display()
            ),
        );
        let got = ask_login_path(&LoginShell::Fish(sh), LOGIN_PATH_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(got.entries()[0], Path::new("/fish/bin"));
        let args = std::fs::read_to_string(args).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "-lic");
        assert!(args[1].contains("(string join : $PATH)"), "{args:?}");
        assert!(args[1].contains("$argv[1]"), "{args:?}");
        assert_eq!(args[2].len(), 16);
    }

    /// A shell that hangs, prints too much, prints no framed value or a malformed one, or is
    /// not there: no login PATH, and why.
    #[tokio::test]
    async fn failing_shells_give_no_login_path() {
        let dir = tempfile::tempdir().unwrap();
        let ask = |body: &str, name: &str| {
            let sh = shell(dir.path(), name, body);
            async move { ask_login_path(&LoginShell::Posix(sh), Duration::from_millis(500)).await }
        };
        let started = std::time::Instant::now();
        let e = ask("sleep 30", "sh").await.unwrap_err();
        assert!(e.contains("did not answer within"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let e = ask("yes banner | head -c 100000", "dash")
            .await
            .unwrap_err();
        assert!(e.contains("printed more than 16 KiB"), "{e}");
        let e = ask("echo hello", "zsh").await.unwrap_err();
        assert_eq!(e, "it printed no PATH");
        let e = ask(
            "printf '__TWR_PATH_%s__/a\\n/b__TWR_PATH_%s__' \"$4\" \"$4\"",
            "ksh",
        )
        .await
        .unwrap_err();
        assert_eq!(e, "its PATH holds a control character");
        let e = ask(
            "printf '__TWR_PATH_%s__relative:./x__TWR_PATH_%s__' \"$4\" \"$4\"",
            "mksh",
        )
        .await
        .unwrap_err();
        assert_eq!(e, "its PATH has no absolute entry");
        let gone = LoginShell::Posix(dir.path().join("no-such-shell"));
        let e = ask_login_path(&gone, LOGIN_PATH_TIMEOUT).await.unwrap_err();
        assert!(e.ends_with("could not be run"), "{e}");
        let e = ask_login_path(
            &LoginShell::of(Some(OsStr::new("/bin/tcsh"))),
            LOGIN_PATH_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(e.starts_with("/bin/tcsh is not asked"), "{e}");
        let e = ask_login_path(&LoginShell::Unset, LOGIN_PATH_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(e, "SHELL is not set");
    }

    /// The resolution contract: a thread's PATH wins; else the login PATH merged before the
    /// runner's; else the runner's (nothing added). `TWR_*_BIN` wins over the command.
    /// One program choice everywhere: an adapter's own binary, else `TWR_*_BIN`, else the
    /// command; an empty value of either counts as unset. The adapters' `bin()` makes the
    /// same choice as the probe, login and exec ([`runtime_program`]).
    #[test]
    fn one_program_choice_and_empty_is_unset() {
        let empty = |_: &str| Some(OsString::new());
        let set = |k: &str| (k == "TWR_CODEX_BIN").then(|| OsString::from("/opt/codex"));
        assert_eq!(runtime_program("codex", empty).unwrap(), Path::new("codex"));
        assert_eq!(
            runtime_program("codex", set).unwrap(),
            Path::new("/opt/codex")
        );
        let own = Path::new("/mine/codex");
        assert_eq!(select_program("codex", Some(own), set).unwrap(), own);
        assert_eq!(
            select_program("codex", Some(Path::new("")), set).unwrap(),
            Path::new("/opt/codex")
        );
        assert_eq!(
            select_program("codex", Some(Path::new("")), empty).unwrap(),
            Path::new("codex")
        );
        let from_env = |k: &str| std::env::var_os(k);
        for (runtime, bin) in [
            ("claude-code", crate::ClaudeCodeAdapter::with_bin("").bin()),
            ("codex", crate::codex::CodexAdapter::with_bin("").bin()),
            ("grok-build", crate::GrokBuildAdapter::with_bin("").bin()),
        ] {
            assert_eq!(Some(bin), runtime_program(runtime, from_env), "{runtime}");
        }
        assert_eq!(
            crate::codex::CodexAdapter::with_bin("/x/codex").bin(),
            Path::new("/x/codex")
        );
    }

    /// The thread's working directory is where relative `PATH` entries are looked up, for the
    /// spawn's resolution and the capability probe's alike (both [`resolve`] with the
    /// thread's `cwd`), whatever the runner's own directory.
    #[test]
    fn relative_path_entries_follow_the_threads_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        install(&bin.join("claude"), "#!/bin/sh\n");
        assert_ne!(std::env::current_dir().unwrap(), dir.path());
        let env = vec![("PATH".to_string(), "bin:/usr/bin".to_string())];
        let r = resolve(Path::new("claude"), &env, None, dir.path());
        assert_eq!(r.found, Some(dir.path().join("bin/claude")));
        assert_eq!(r.path.as_deref(), Some(OsStr::new("bin:/usr/bin")));
        let env = vec![("PATH".to_string(), ":/usr/bin".to_string())];
        install(&dir.path().join("codex"), "#!/bin/sh\n");
        let r = resolve(Path::new("codex"), &env, None, dir.path());
        assert_eq!(r.found, Some(dir.path().join("codex")));
    }

    #[test]
    fn the_resolution_contract() {
        let dir = tempfile::tempdir().unwrap();
        let (login_bin, thread_bin) = (dir.path().join("login"), dir.path().join("thread"));
        for d in [&login_bin, &thread_bin] {
            std::fs::create_dir(d).unwrap();
            install(&d.join("claude"), "#!/bin/sh\n");
        }
        let login = LoginPath::parse(login_bin.to_str().unwrap().as_bytes()).unwrap();
        let program = runtime_program("claude-code", |_| None).unwrap();
        assert_eq!(program, Path::new("claude"));

        // No thread PATH: the login PATH's, merged before the runner's.
        let r = resolve(&program, &[], Some(&login), Path::new(""));
        assert_eq!(r.found, Some(login_bin.join("claude")));
        let path = r.path.unwrap();
        assert!(path
            .to_str()
            .unwrap()
            .starts_with(login_bin.to_str().unwrap()));
        assert_eq!(path, runtime_path(&[], Some(&login)).unwrap());

        // The thread's PATH wins over the login PATH.
        let env = vec![("PATH".to_string(), thread_bin.display().to_string())];
        let r = resolve(&program, &env, Some(&login), Path::new(""));
        assert_eq!(r.found, Some(thread_bin.join("claude")));
        assert_eq!(r.path.as_deref(), Some(thread_bin.as_os_str()));

        // A runner put the login PATH into the environment: the adapter's resolution (no
        // login) finds the same file the probe's (empty env with the login) does.
        let mut spawned = Vec::new();
        with_login_path(&mut spawned, Some(&login));
        let probe = resolve(&program, &[], Some(&login), Path::new(""));
        let spawn = resolve(&program, &spawned, None, Path::new(""));
        assert_eq!(spawn, probe);
        // ... and leaves a thread's own PATH alone.
        let mut own = env.clone();
        with_login_path(&mut own, Some(&login));
        assert_eq!(own, env);
        // No login PATH: nothing added, the runner's PATH (a hub's local box).
        let mut none = Vec::new();
        with_login_path(&mut none, None);
        assert!(none.is_empty());
        assert_eq!(resolve(&program, &[], None, Path::new("")).path, None);

        // TWR_CLAUDE_BIN wins, a path as given, a bare name on the PATH.
        let pinned = dir.path().join("pinned-claude");
        install(&pinned, "#!/bin/sh\n");
        let var = |k: &str| (k == "TWR_CLAUDE_BIN").then(|| pinned.clone().into_os_string());
        let program = runtime_program("claude-code", var).unwrap();
        assert_eq!(program, pinned);
        let r = resolve(&program, &env, Some(&login), Path::new(""));
        assert_eq!(r.found, Some(pinned.clone()));
        assert_eq!(runtime_program("codex", var).unwrap(), Path::new("codex"));
        assert_eq!(runtime_program("acp", var), None);
        let named = |k: &str| (k == "TWR_GROK_BIN").then(|| OsString::from("claude"));
        let program = runtime_program("grok-build", named).unwrap();
        let r = resolve(&program, &[], Some(&login), Path::new(""));
        assert_eq!(r.found, Some(login_bin.join("claude")));
        // Not there: none found.
        let r = resolve(Path::new("no-such-agent"), &env, None, Path::new(""));
        assert_eq!(r.found, None);
    }
}

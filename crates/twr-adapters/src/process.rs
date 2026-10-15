//! Process plumbing shared by the adapters: the bounded stdout line reader of the headless
//! adapters and the environment scrubbing every runtime (and `attach`) command gets.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// twr secrets a runtime process must never inherit: the runner's hub token, the
/// orchestrator token, and a test's user token (`TWR_TEST_USER_TOKEN_FILE`, which makes a
/// test's clients the user of its throwaway hub; an agent of that hub is no such client). The
/// orchestrator's MCP server gets its token through the MCP configuration (a file or stdin),
/// never through the runtime's environment.
pub const SCRUBBED_ENV: &[&str] = &[
    "TWR_RUNNER_TOKEN",
    "TWR_ORCH_TOKEN",
    twr_core::client::TEST_USER_TOKEN_ENV,
];

/// Removes [`SCRUBBED_ENV`] from a runtime command (whether inherited or set on it).
pub fn scrub(cmd: &mut tokio::process::Command) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// A command that runs `program` under `wrap` ([`twr_core::adapter::SpawnConfig::wrap`]):
/// `wrap`'s first word with the rest of it, then `program`; `program` itself when `wrap` is
/// empty. The process started keeps its id when the wrapper runs `program` in its place
/// (`systemd-run --scope`, `nice` do).
pub fn wrapped(wrap: &[String], program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    match wrap.split_first() {
        Some((first, rest)) => {
            let mut cmd = tokio::process::Command::new(first);
            cmd.args(rest).arg(program);
            cmd
        }
        None => tokio::process::Command::new(program),
    }
}

/// Before starting `program` in `cwd` with `env` added under a non-empty `wrap`
/// ([`wrapped`]): the error [`start_error`] gives for a program that is not there, which the
/// wrapper would otherwise hide (it starts, then fails to run the program, and the runtime
/// reads as crashed). A bare name is looked up on `env`'s `PATH` (its last), else the hub's,
/// as the program itself would be. `Ok` without a wrap, or when the program is there.
pub fn check_wrapped(
    wrap: &[String],
    program: &std::path::Path,
    env: &[(String, String)],
    cwd: &std::path::Path,
    fix: &str,
) -> anyhow::Result<()> {
    if wrap.is_empty() || !(cwd.as_os_str().is_empty() || cwd.is_dir()) {
        return Ok(());
    }
    let base = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        cwd.to_path_buf()
    };
    let path = env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os("PATH"));
    match missing_program(program, path.as_deref(), &base) {
        Some(why) => Err(anyhow::anyhow!("{why} ({fix})")),
        None => Ok(()),
    }
}

/// [`scrub`] for a `std` command (`attach`).
pub fn scrub_std(cmd: &mut std::process::Command) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// The variables of `cfg.env` an interactive command (`attach`) runs with, as its runtime
/// does: the configured ones of a worker thread (`[threads] env`, `[projects.<name>] env`), not
/// twr's own `TWR_*` entries (a runtime's command, the markers of a runtime process). Set before
/// [`scrub_std`].
pub fn attach_env(cfg: &twr_core::adapter::SpawnConfig) -> impl Iterator<Item = (&str, &str)> {
    cfg.env
        .iter()
        .filter(|(k, _)| !k.to_ascii_uppercase().starts_with("TWR_"))
        .map(|(k, v)| (k.as_str(), v.as_str()))
}

/// [`scrub`] for a PTY command (portable-pty copies the whole environment into it).
pub fn scrub_pty(cmd: &mut portable_pty::CommandBuilder) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// Why runtime program `program` (an argv[0]) cannot be run, when it cannot: a bare name
/// (`claude`) that is on no directory of `path` (a `PATH` value) as an executable file, or a
/// path (`./bin/agent`, `/opt/agent`; a relative one from `cwd`, where the program starts)
/// that is not an executable file. Leads with the program: "`claude` not found on PATH". `None`
/// when it can be run, or `program` is empty.
pub fn missing_program(
    program: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    if program.as_os_str().is_empty() {
        return None;
    }
    let runnable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let shown = program.display();
    if program.components().count() > 1 || program.is_absolute() {
        let p = cwd.join(program);
        return match std::fs::metadata(&p) {
            _ if runnable(&p) => None,
            Ok(_) => Some(format!("`{shown}` is not an executable file")),
            Err(_) => Some(format!("`{shown}` not found")),
        };
    }
    let found = find_program(program, path, cwd).is_some();
    (!found).then(|| format!("`{shown}` not found on PATH"))
}

/// The executable file runtime program `program` (an argv[0]) names: a bare name looked up on
/// the directories of `path` (a `PATH` value), a path taken from `cwd`. `None` when there is
/// no such executable file, or `program` is empty.
pub fn find_program(
    program: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if program.as_os_str().is_empty() {
        return None;
    }
    let runnable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.components().count() > 1 || program.is_absolute() {
        let p = cwd.join(program);
        return runnable(&p).then_some(p);
    }
    std::env::split_paths(path?)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(program))
        .find(|p| runnable(p))
}

/// The error of starting runtime program `program` in `cwd` (`e`, from spawning it). A program
/// that is not there ([`missing_program`] with the hub's `PATH`) leads with its name and `fix`,
/// how to install it or point twr at another one: "`claude` not found on PATH (install Claude
/// Code, or set TWR_CLAUDE_BIN)". Anything else, a missing working directory included, is
/// "starting <program> in <cwd>: <e>".
pub fn start_error(
    e: io::Error,
    program: &std::path::Path,
    cwd: &std::path::Path,
    fix: &str,
) -> anyhow::Error {
    let cwd_ok = cwd.as_os_str().is_empty() || cwd.is_dir();
    let base = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        cwd.to_path_buf()
    };
    let path = std::env::var_os("PATH");
    if let Some(why) = (e.kind() == io::ErrorKind::NotFound && cwd_ok)
        .then(|| missing_program(program, path.as_deref(), &base))
        .flatten()
    {
        return anyhow::anyhow!("{why} ({fix})");
    }
    anyhow::Error::new(e).context(format!(
        "starting {} in {}",
        program.display(),
        cwd.display()
    ))
}

/// Whether `key` is one of [`SCRUBBED_ENV`].
pub fn is_scrubbed(key: &str) -> bool {
    SCRUBBED_ENV.contains(&key)
}

/// Longest stdout line a headless adapter buffers (32 MiB). A longer line is skipped up to its
/// newline and reported as [`Line::Oversized`].
pub const MAX_LINE: usize = 32 * 1024 * 1024;

/// Bytes kept of an oversized line (enough to tell what kind of message it was).
pub const OVERSIZED_HEAD: usize = 4096;

/// One stdout line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A complete line (without `\n` / `\r\n`; invalid UTF-8 replaced).
    Text(String),
    /// A line longer than the cap: only its first [`OVERSIZED_HEAD`] bytes were kept.
    Oversized {
        /// Its full length in bytes.
        len: usize,
        /// Its start.
        head: String,
    },
}

/// Reads newline-delimited lines without ever buffering more than `max` bytes of one line.
///
/// Cancel safe: everything read so far is kept in `self`, so dropping a pending
/// [`BoundedLines::next_line`] (as `tokio::select!` does) loses nothing.
#[derive(Debug)]
pub struct BoundedLines<R> {
    reader: BufReader<R>,
    max: usize,
    buf: Vec<u8>,
    /// Bytes of the current line so far.
    len: usize,
    oversized: u64,
}

impl<R: AsyncRead + Unpin> BoundedLines<R> {
    /// Lines of `r`, each at most `max` bytes.
    pub fn new(r: R, max: usize) -> Self {
        BoundedLines {
            reader: BufReader::new(r),
            max,
            buf: Vec::new(),
            len: 0,
            oversized: 0,
        }
    }

    /// How many oversized lines were skipped so far.
    pub fn oversized_count(&self) -> u64 {
        self.oversized
    }

    /// The next line, or None at EOF.
    pub async fn next_line(&mut self) -> io::Result<Option<Line>> {
        loop {
            let avail = self.reader.fill_buf().await?;
            if avail.is_empty() {
                if self.len == 0 {
                    return Ok(None);
                }
                return Ok(Some(self.take()));
            }
            let (chunk, found) = match avail.iter().position(|b| *b == b'\n') {
                Some(i) => (&avail[..i], true),
                None => (avail, false),
            };
            let n = chunk.len();
            let was_over = self.len > self.max;
            self.len += n;
            if self.len <= self.max {
                self.buf.extend_from_slice(chunk);
            } else if !was_over {
                // Just crossed the cap: keep the head only and drop the rest of the line.
                let keep = OVERSIZED_HEAD.saturating_sub(self.buf.len()).min(n);
                self.buf.extend_from_slice(&chunk[..keep]);
                self.buf.truncate(OVERSIZED_HEAD);
                self.buf.shrink_to_fit();
            }
            self.reader.consume(n + usize::from(found));
            if found {
                return Ok(Some(self.take()));
            }
        }
    }

    fn take(&mut self) -> Line {
        let len = std::mem::take(&mut self.len);
        let mut bytes = std::mem::take(&mut self.buf);
        if len > self.max {
            self.oversized += 1;
            tracing::warn!(
                len,
                max = self.max,
                oversized_lines = self.oversized,
                "skipping an oversized runtime stdout line"
            );
            return Line::Oversized {
                len,
                head: String::from_utf8_lossy(&bytes).into_owned(),
            };
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Line::Text(match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }
}

/// Whether the head of an oversized JSON line has `"key":"value"` (compact or with a space
/// after the colon).
pub fn head_has(head: &str, key: &str, value: &str) -> bool {
    head.contains(&format!("\"{key}\":\"{value}\""))
        || head.contains(&format!("\"{key}\": \"{value}\""))
}

/// The first numeric `"id"` of the head of an oversized JSON-RPC message, when no `"method"`
/// comes before it (so it is a response to one of twr's requests).
pub fn head_response_id(head: &str) -> Option<u64> {
    let at = head.find("\"id\":")?;
    if head[..at].contains("\"method\"") {
        return None;
    }
    let rest = head[at + 5..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Under a wrapper, a runtime program that is not there is still said to be missing,
    /// with what to do.
    #[test]
    fn a_wrapped_missing_program_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let wrap = ["nice".to_string()];
        let e = check_wrapped(
            &wrap,
            Path::new("./no-agent"),
            &[],
            dir.path(),
            "install it",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(e, "`./no-agent` not found (install it)");
        assert!(check_wrapped(&[], Path::new("./no-agent"), &[], dir.path(), "x").is_ok());
        assert!(check_wrapped(&wrap, Path::new("sh"), &[], dir.path(), "x").is_ok());
        // A bare name on the worker's own PATH (`[threads] env`) is there.
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("only-here"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.join("only-here"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let env = [("PATH".to_string(), bin.to_string_lossy().into_owned())];
        assert!(check_wrapped(&wrap, Path::new("only-here"), &env, dir.path(), "x").is_ok());
        let e = check_wrapped(&wrap, Path::new("only-here"), &[], dir.path(), "x").unwrap_err();
        assert_eq!(e.to_string(), "`only-here` not found on PATH (x)");
    }

    /// A runtime's command under a wrapper is the wrapper's, the runtime's program last.
    #[test]
    fn a_wrapped_command_runs_the_wrapper_first() {
        let wrap = ["nice".to_string(), "-n".to_string(), "7".to_string()];
        let cmd = wrapped(&wrap, "/usr/bin/agent");
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "nice");
        let args: Vec<_> = std.get_args().collect();
        assert_eq!(args, ["-n", "7", "/usr/bin/agent"]);
        let bare = wrapped(&[], "/usr/bin/agent");
        assert_eq!(bare.as_std().get_program(), "/usr/bin/agent");
        assert_eq!(bare.as_std().get_args().count(), 0);
    }

    /// A program that is not there is named first (bare names looked up on
    /// PATH, paths checked from the program's directory).
    #[test]
    fn missing_programs_are_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let agent = bin.join("agent");
        std::fs::write(&agent, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(bin.join("plain"), "").unwrap();
        let path = std::env::join_paths(["/nonexistent", bin.to_str().unwrap()]).unwrap();
        let path = Some(path.as_os_str());
        let cwd = dir.path();
        let missing = |p: &str, path| missing_program(Path::new(p), path, cwd);
        assert_eq!(missing("agent", path), None);
        assert_eq!(missing("bin/agent", path), None);
        assert_eq!(missing("./bin/agent", None), None);
        assert_eq!(missing(agent.to_str().unwrap(), None), None);
        assert_eq!(missing("", path), None);
        assert_eq!(
            missing("no-such-agent", path).as_deref(),
            Some("`no-such-agent` not found on PATH")
        );
        assert!(missing("agent", None).is_some());
        // Not executable, or a directory: not runnable either.
        assert!(missing("plain", path).is_some());
        let dirs = Some(dir.path().as_os_str());
        assert!(missing_program(Path::new("bin"), dirs, cwd).is_some());
        assert_eq!(
            missing("./missing/agent", path).as_deref(),
            Some("`./missing/agent` not found")
        );
        assert_eq!(
            missing("bin/plain", path).as_deref(),
            Some("`bin/plain` is not an executable file")
        );
        // The file a program names.
        assert_eq!(
            find_program(Path::new("agent"), path, cwd),
            Some(agent.clone())
        );
        assert_eq!(
            find_program(Path::new("bin/agent"), None, cwd),
            Some(cwd.join("bin/agent"))
        );
        assert_eq!(find_program(Path::new("plain"), path, cwd), None);
        assert_eq!(find_program(Path::new("agent"), None, cwd), None);
        assert_eq!(find_program(Path::new(""), path, cwd), None);
    }

    /// The spawn error of a missing program says what to do; others keep the
    /// program, directory and cause.
    #[test]
    fn start_errors_lead_with_a_missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let not_found = || io::Error::from(io::ErrorKind::NotFound);
        let fix = "install Claude Code, or set TWR_CLAUDE_BIN";
        let e = start_error(
            not_found(),
            Path::new("twr-test-no-such-cli"),
            dir.path(),
            fix,
        );
        assert_eq!(
            format!("{e:#}"),
            "`twr-test-no-such-cli` not found on PATH (install Claude Code, or set \
             TWR_CLAUDE_BIN)"
        );
        let e = start_error(
            not_found(),
            Path::new("/nonexistent/claude"),
            dir.path(),
            fix,
        );
        assert!(format!("{e:#}").starts_with("`/nonexistent/claude` not found ("));
        // A missing working directory is not a missing program.
        let gone = dir.path().join("gone");
        let e = start_error(not_found(), Path::new("twr-test-no-such-cli"), &gone, fix);
        let e = format!("{e:#}");
        assert!(e.starts_with("starting twr-test-no-such-cli in "), "{e}");
        assert!(!e.contains(fix), "{e}");
        // Nor is a program that is there (a missing interpreter, say), or another error.
        let e = start_error(not_found(), Path::new("sh"), dir.path(), fix);
        assert!(format!("{e:#}").starts_with("starting sh in "));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let e = start_error(denied, Path::new("twr-test-no-such-cli"), dir.path(), fix);
        assert!(format!("{e:#}").starts_with("starting twr-test-no-such-cli in "));
    }

    async fn all(input: &[u8], max: usize) -> Vec<Line> {
        let mut r = BoundedLines::new(input, max);
        let mut out = Vec::new();
        while let Some(l) = r.next_line().await.unwrap() {
            out.push(l);
        }
        out
    }

    fn text(s: &str) -> Line {
        Line::Text(s.into())
    }

    #[tokio::test]
    async fn lines_within_the_cap() {
        assert_eq!(
            all(b"a\nbb\r\n\nlast", 8).await,
            [text("a"), text("bb"), text(""), text("last")]
        );
        assert_eq!(all(b"", 8).await, []);
        // Invalid UTF-8 does not end the stream.
        assert_eq!(
            all(b"a\xffb\nok\n", 8).await,
            [text("a\u{fffd}b"), text("ok")]
        );
        // Exactly the cap is fine.
        assert_eq!(all(b"12345678\n", 8).await, [text("12345678")]);
    }

    #[tokio::test]
    async fn an_oversized_line_is_skipped_to_its_newline() {
        let mut input = b"{\"type\":\"result\"".to_vec();
        input.extend(std::iter::repeat_n(b'x', 10_000));
        input.extend(b"\nnext\n");
        let got = all(&input, 64).await;
        assert_eq!(got.len(), 2);
        match &got[0] {
            Line::Oversized { len, head } => {
                assert_eq!(*len, 10_016);
                assert!(head.starts_with("{\"type\":\"result\""), "{head}");
                assert!(head.len() <= OVERSIZED_HEAD);
                assert!(head_has(head, "type", "result"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(got[1], text("next"));
        // Unterminated at EOF: still reported, never buffered past the head.
        let got = all(&[b'y'; 5000], 16).await;
        assert!(matches!(&got[..], [Line::Oversized { len: 5000, .. }]));
    }

    #[tokio::test]
    async fn memory_stays_bounded_while_skipping() {
        // A reader fed in small pieces keeps at most the head of a huge line.
        let (mut w, r) = tokio::io::duplex(64);
        let mut lines = BoundedLines::new(r, 100);
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for _ in 0..1000 {
                w.write_all(&[b'z'; 1000]).await.unwrap();
            }
            wait.await.unwrap();
            w.write_all(b"\nok\n").await.unwrap();
        });
        // Read (and cancel, as `select!` does) until the whole unterminated line went by.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while lines.len < 1_000_000 {
                let r =
                    tokio::time::timeout(std::time::Duration::from_millis(5), lines.next_line())
                        .await;
                assert!(r.is_err(), "no line yet");
            }
        })
        .await
        .unwrap();
        assert!(
            lines.buf.capacity() <= OVERSIZED_HEAD,
            "{}",
            lines.buf.capacity()
        );
        go.send(()).unwrap();
        let first = lines.next_line().await.unwrap().unwrap();
        assert!(matches!(first, Line::Oversized { len: 1_000_000, .. }));
        assert_eq!(lines.next_line().await.unwrap(), Some(text("ok")));
        assert_eq!(lines.oversized_count(), 1);
        writer.await.unwrap();
    }

    #[test]
    fn response_ids() {
        assert_eq!(
            head_response_id(r#"{"jsonrpc":"2.0","id":7,"result":{"#),
            Some(7)
        );
        assert_eq!(
            head_response_id(r#"{"jsonrpc":"2.0","method":"x","id":7"#),
            None
        );
        assert_eq!(head_response_id(r#"{"id":"s","result":1"#), None);
    }

    #[test]
    fn scrubbing() {
        let mut cmd = tokio::process::Command::new("true");
        cmd.env("TWR_ORCH_TOKEN", "x").env("KEEP", "1");
        scrub(&mut cmd);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        for k in SCRUBBED_ENV {
            assert!(envs.contains(&(std::ffi::OsStr::new(k), None)), "{k}");
        }
        assert!(envs.contains(&(
            std::ffi::OsStr::new("KEEP"),
            Some(std::ffi::OsStr::new("1"))
        )));
        assert!(is_scrubbed("TWR_RUNNER_TOKEN") && !is_scrubbed("TWR_THREAD"));
    }
}

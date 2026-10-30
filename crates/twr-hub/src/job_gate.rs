//! A job's start gate: the process a job's command starts as, which runs nothing of the job's
//! until the hub has recorded its process group (`hub::jobs`), and then becomes the command
//! (`execv`: the same process, pid, process group and start time).
//!
//! The hub runs it as `<twr> __job-gate -- <argv...>` ([`GATE_ARG`]; `twr-job-gate -- ...` in
//! this crate's tests), in the job's directory, with the job's environment as its own process
//! environment (only its user may read it, unlike its arguments), its output to the job's log,
//! as the leader of a session and process group of its own (no controlling terminal), and with
//! its standard input one end of a Unix socket pair whose other end the hub keeps. Over that
//! socket, one line each way at a time:
//!
//! 1. The gate finds the program as `execvp` would: a name with a `/` as it is; else each
//!    directory of its own `PATH` (`/bin:/usr/bin` without one) in turn, an empty entry
//!    meaning its working directory and a relative one taken from it, the first executable
//!    file there. It says `ready`, or `missing <why>` and exits 127.
//! 2. It waits for `go`. End of file, or anything else, and it exits 125 having run nothing.
//!    The hub kills it rather than let it read either when the gate does not open.
//! 3. It keeps a close-on-exec copy of the socket, puts `/dev/null` on its standard input,
//!    restores `SIGPIPE` to its default (Rust ignores it, and an ignored signal stays ignored
//!    across `exec`) and `execv`s the program found with the job's argv, its first word as
//!    given (no option of a shell's `exec` applies to it). Should that fail, it says
//!    `exec <errno>` on the socket and exits 127; when it succeeds the hub reads end of file
//!    there, as `std::process::Command` tells a failed `exec` from a started program.
//!
//! Nothing of the job's environment is interpreted on the way, and every variable reaches the
//! command as given.

use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// The argument `twr` runs a job's start gate with: `twr __job-gate -- <argv...>`.
pub const GATE_ARG: &str = "__job-gate";

/// What `PATH` is taken to be without one (glibc's and BSD's `execvp` default, near enough).
const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// Runs a job's start gate (see the module docs) with `args`, the gate's arguments after its
/// program: [`GATE_ARG`], `--`, then the job's argv. Never returns.
pub fn job_gate_main(args: Vec<OsString>) -> ! {
    let mut sock = std::mem::ManuallyDrop::new(stdin_socket());
    let argv = match args.as_slice() {
        [gate, dashes, argv @ ..] if gate == GATE_ARG && dashes == "--" && !argv.is_empty() => {
            argv.to_vec()
        }
        _ => {
            let _ = writeln!(&mut *sock, "missing the gate was called without a command");
            std::process::exit(127)
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = std::env::var_os("PATH");
    let program = match resolve(&argv[0], path.as_deref(), &cwd) {
        Ok(p) => p,
        Err(why) => {
            let _ = writeln!(&mut *sock, "missing {why}");
            std::process::exit(127)
        }
    };
    if sock.write_all(b"ready\n").is_err() {
        std::process::exit(125);
    }
    if read_line(&mut *sock).as_deref() != Some(b"go".as_slice()) {
        std::process::exit(125);
    }
    let failed = exec(&program, &argv);
    let _ = writeln!(&mut *sock, "exec {failed}");
    std::process::exit(127)
}

/// The socket on standard input, as a file (not closed when dropped: see the caller).
fn stdin_socket() -> std::fs::File {
    use std::os::fd::FromRawFd;
    // SAFETY: descriptor 0 is the gate's socket, which nothing else in this process uses; the
    // caller never drops the file, so the descriptor is closed only by `exec` or `exit`.
    unsafe { std::fs::File::from_raw_fd(0) }
}

/// One line from `r`, without its `\n`: at most 64 bytes; `None` at end of file first.
fn read_line(r: &mut impl Read) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 64 {
        match r.read(&mut b) {
            Ok(0) => return None,
            Ok(_) if b[0] == b'\n' => return Some(line),
            Ok(_) => line.push(b[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    None
}

/// The file `execvp` would run for `program` with `path` (a `PATH` value) from `cwd` (see the
/// module docs), or why there is none.
pub fn resolve(program: &OsStr, path: Option<&OsStr>, cwd: &Path) -> Result<PathBuf, String> {
    let shown = Path::new(program).display().to_string();
    if program.is_empty() {
        return Err("the command is empty".into());
    }
    if program.as_bytes().contains(&b'/') {
        let p = PathBuf::from(program);
        return match std::fs::metadata(cwd.join(&p)) {
            Ok(_) => Ok(from_here(p)),
            Err(e) => Err(format!("`{shown}`: {e}")),
        };
    }
    let path = path.unwrap_or(OsStr::new(DEFAULT_PATH));
    for dir in path.as_bytes().split(|b| *b == b':') {
        let dir = Path::new(OsStr::from_bytes(dir));
        let candidate = if dir.as_os_str().is_empty() {
            PathBuf::from(program)
        } else {
            dir.join(program)
        };
        if executable(&cwd.join(&candidate)) {
            return Ok(from_here(candidate));
        }
    }
    Err(format!("`{shown}` not found on PATH"))
}

/// `p` as the gate execs it: an absolute path as it is, a relative one from the working
/// directory spelled with a leading `./`, so that it starts with no `-` (a script's
/// interpreter gets the path as an argument, and would take `-a/x` for options).
fn from_here(p: PathBuf) -> PathBuf {
    if p.is_absolute() || p.starts_with(".") || p.starts_with("..") {
        p
    } else {
        Path::new(".").join(p)
    }
}

/// Whether `p` is a file this process may execute.
fn executable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access reads the NUL-terminated path it is given.
    let ok = unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0;
    ok && std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// Becomes `program` with `argv` (keeping a close-on-exec copy of the socket on descriptor 0
/// for a failure, putting `/dev/null` on it, `SIGPIPE` back to its default). Returns only when
/// that fails: the error number, or 0 when it could not even be tried.
fn exec(program: &Path, argv: &[OsString]) -> i32 {
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let Ok(prog) = CString::new(program.as_os_str().as_bytes()) else {
        return libc::EINVAL;
    };
    let Ok(args) = argv
        .iter()
        .map(|a| CString::new(a.clone().into_vec()))
        .collect::<Result<Vec<_>, _>>()
    else {
        return libc::EINVAL;
    };
    let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: plain descriptor and signal calls on this single-threaded process's own
    // descriptors; `execv` gets NUL-terminated strings and a NULL-terminated array that
    // outlive the call.
    unsafe {
        let status = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3);
        if status < 0 {
            return errno();
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if null < 0 || libc::dup2(null, 0) < 0 {
            return errno();
        }
        libc::close(null);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::execv(prog.as_ptr(), ptrs.as_ptr());
        let e = errno();
        // Say it on the socket's copy: descriptor 0 is `/dev/null` now.
        let line = format!("exec {e}\n");
        libc::write(status, line.as_ptr().cast(), line.len());
        libc::_exit(127)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program is found as `execvp` finds it: a name with `/` as it is, else on `PATH` from
    /// the working directory (an empty entry is the directory itself, a relative one is taken
    /// from it), executable files only.
    #[test]
    fn programs_are_found_as_execvp_finds_them() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("jest");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let p = |s: &str| Some(OsString::from(s));
        let r = |prog: &str, path: Option<OsString>| {
            resolve(OsStr::new(prog), path.as_deref(), dir.path())
        };
        assert_eq!(
            r("jest", p("/nonexistent:node_modules/.bin:/bin")),
            Ok(PathBuf::from("./node_modules/.bin/jest"))
        );
        assert_eq!(
            r("jest", p(&format!("{}", bin.display()))),
            Ok(tool.clone())
        );
        assert!(r("jest", p("/bin"))
            .unwrap_err()
            .contains("not found on PATH"));
        // Not executable: not a candidate.
        assert!(r("plain", p("")).is_err());
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r("plain", p(":/bin")), Ok(PathBuf::from("./plain")));
        // With a `/`: as given, if it is there.
        assert_eq!(
            r("-a/x", None).unwrap_err().split(':').next(),
            Some("`-a/x`")
        );
        assert_eq!(r("./plain", None), Ok(PathBuf::from("./plain")));
        std::fs::create_dir(dir.path().join("-a")).unwrap();
        std::fs::write(dir.path().join("-a/x"), "x").unwrap();
        assert_eq!(r("-a/x", None), Ok(PathBuf::from("./-a/x")));
        assert!(r("", None).is_err());
        assert!(r("sh", None).is_ok());
    }
}

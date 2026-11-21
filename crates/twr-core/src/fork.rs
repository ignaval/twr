//! Starting a process through `fork`, which a command with a `pre_exec` closure does (Rust
//! uses `posix_spawn`, which forks nothing in this process, for every other command).
//!
//! On macOS the child of a `fork` runs libSystem's fork handlers before it can exec, and
//! libnotify's (`_notify_fork_child`) reads libnotify's globals, set up once per process on
//! first use. A process forked while another of its threads is in the middle of that set-up
//! inherits it half done, with no thread to finish it: the handler aborts and the child dies
//! of SIGKILL (`EXC_BREAKPOINT`, `_os_once_gate_corruption_abort`) before it execs. Its
//! `spawn` has already succeeded by then, so what it was to run (`git`, `ps`, a probe) seems
//! to have been killed by a signal. Much of libSystem sets libnotify up on first use (os_log
//! and os_activity, `confstr(_CS_DARWIN_USER_TEMP_DIR)`, user lookups), on any thread.
//! [`prepare_fork`] finishes that set-up before the first fork.

/// Call before spawning a command that has a `pre_exec` closure (it forks this process). On
/// macOS it sets libnotify up, once per process, waiting for a thread that is setting it up
/// meanwhile: no fork after it can catch that half done ([module docs](self)). Elsewhere it
/// does nothing.
pub fn prepare_fork() {
    #[cfg(target_os = "macos")]
    {
        static READY: std::sync::Once = std::sync::Once::new();
        extern "C" {
            // <notify.h>, in libSystem. It reads libnotify's globals (setting them up the
            // first time) and talks to no daemon.
            fn notify_is_valid_token(val: libc::c_int) -> bool;
        }
        // SAFETY: takes an int, touches no memory of ours.
        READY.call_once(|| unsafe {
            notify_is_valid_token(0);
        });
    }
}

/// Marks every file descriptor above stderr close-on-exec, for a `pre_exec` closure (between
/// fork and exec): the program started gets only its stdin, stdout and stderr. A process that
/// outlives the one that starts it would otherwise keep, for as long as it runs, whatever that
/// one inherited without close-on-exec. The hub is such a process, and the desktop app runs
/// the `twr` that starts it with dozens of Electron's descriptors (GPU and profile files,
/// Chromium's IPC sockets, the crash reporter's socket, which keeps the crash reporter, and
/// with it the app's stdout, alive after the app quits). Allocates nothing and calls only
/// async-signal-safe functions; it cannot fail.
pub fn close_inherited_on_exec() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // close_range(2) (Linux 5.11 and later) marks them all in one call.
        // SAFETY: takes three integers and touches no memory of this process.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if marked == 0 {
            return Ok(());
        }
    }
    // An older kernel, or macOS: each descriptor in turn, up to the limit (at most 65536).
    // SAFETY: sysconf and fcntl take integers and touch no memory of this process.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let limit = if limit <= 0 { 1024 } else { limit.min(65536) } as libc::c_int;
    for fd in 3..limit {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    /// Whether a child started with `close` sees descriptor `fd`, which this process holds
    /// open without close-on-exec.
    fn child_sees(fd: libc::c_int, close: bool) -> bool {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("[ -e /dev/fd/{fd} ]"));
        if close {
            super::prepare_fork();
            // SAFETY: close_inherited_on_exec is async-signal-safe and allocates nothing.
            unsafe {
                cmd.pre_exec(super::close_inherited_on_exec);
            }
        }
        cmd.status().unwrap().success()
    }

    #[test]
    fn a_child_gets_no_inherited_descriptor_but_its_stdio() {
        // A descriptor as Electron leaves them: open across exec.
        // SAFETY: opens /dev/null; the descriptor is closed below.
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        assert!(fd > 2);
        assert!(child_sees(fd, false), "the control child inherits it");
        assert!(!child_sees(fd, true));
        // SAFETY: closes the descriptor opened above.
        unsafe { libc::close(fd) };
    }
}

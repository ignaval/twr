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

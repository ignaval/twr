//! Terminal-safe output. What the CLI prints includes text agents control (attention
//! summaries and options, thread names, prompts, replies, reports, hub errors quoting them):
//! control characters in it are escaped, so it cannot move the cursor, clear the screen, set
//! the window title or otherwise drive the terminal. Every `print!`-style output of the CLI
//! goes through [`out!`] / [`outln!`] / [`err!`] / [`errln!`] (or [`clean`] directly).

use std::borrow::Cow;

/// Whether `c` is escaped by [`clean`]: C0 controls except newline and tab, DEL, and C1
/// controls.
fn escaped(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c)
}

/// `s` with every control character but newline and tab written as `\uXXXX` (also valid
/// inside a JSON string, so JSON output stays JSON and means the same).
pub fn clean(s: &str) -> Cow<'_, str> {
    if !s.chars().any(escaped) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if escaped(c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `print!` through [`clean`].
macro_rules! out {
    ($($arg:tt)*) => {
        ::std::print!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `println!` through [`clean`].
macro_rules! outln {
    () => {
        ::std::println!()
    };
    ($($arg:tt)*) => {
        ::std::println!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `eprint!` through [`clean`].
macro_rules! err {
    ($($arg:tt)*) => {
        ::std::eprint!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `eprintln!` through [`clean`].
macro_rules! errln {
    ($($arg:tt)*) => {
        ::std::eprintln!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// Longest line [`read_hidden_line`] takes, in bytes.
pub const HIDDEN_LINE_MAX: usize = 1024;

/// What a byte typed at [`read_hidden_line`]'s prompt did ([`HiddenLine::push`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// Taken (or dropped): read on.
    More,
    /// Enter: the line is complete.
    Done,
    /// Ctrl-C, or Ctrl-D on an empty line.
    Cancelled,
}

/// Where [`HiddenLine`] is in an escape sequence (an arrow key, a bracketed paste's markers),
/// which it drops whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Escape {
    #[default]
    None,
    /// After ESC.
    Started,
    /// In `ESC [ ...`, up to its final byte (`@` to `~`).
    Csi,
    /// After `ESC O`: one more byte.
    Ss3,
}

/// The line typed at [`read_hidden_line`]'s prompt, byte by byte, as a terminal's line editing
/// would make it with echo off: Enter (`\r` or `\n`) ends it; Ctrl-C, and Ctrl-D on an empty
/// line, cancel; DEL and Backspace remove the last byte, Ctrl-U all of them; escape sequences
/// and other control bytes are dropped. More than [`HIDDEN_LINE_MAX`] bytes is an error.
#[derive(Debug, Default)]
pub struct HiddenLine {
    buf: Vec<u8>,
    escape: Escape,
}

impl HiddenLine {
    /// Takes byte `b`.
    pub fn push(&mut self, b: u8) -> anyhow::Result<Typed> {
        match (self.escape, b) {
            (_, 3) => return Ok(Typed::Cancelled),
            (Escape::Started, b'[') => self.escape = Escape::Csi,
            (Escape::Started, b'O') => self.escape = Escape::Ss3,
            (Escape::Started | Escape::Ss3, _) => self.escape = Escape::None,
            (Escape::Csi, 0x40..=0x7e) => self.escape = Escape::None,
            (Escape::Csi, _) => {}
            (Escape::None, b'\r' | b'\n') => return Ok(Typed::Done),
            (Escape::None, 4) if self.buf.is_empty() => return Ok(Typed::Cancelled),
            (Escape::None, 0x7f | 8) => {
                self.buf.pop();
            }
            (Escape::None, 0x15) => self.buf.clear(),
            (Escape::None, 0x1b) => self.escape = Escape::Started,
            (Escape::None, 0..=0x1f) => {}
            (Escape::None, b) => {
                if self.buf.len() >= HIDDEN_LINE_MAX {
                    anyhow::bail!("the line is longer than {HIDDEN_LINE_MAX} characters");
                }
                self.buf.push(b);
            }
        }
        Ok(Typed::More)
    }

    /// The line so far (bytes that are not UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

/// Zeroes its buffer when dropped. Best effort only: [`HiddenLine::text`] returns a copy,
/// which its caller keeps.
impl Drop for HiddenLine {
    fn drop(&mut self) {
        self.buf.iter_mut().for_each(|b| *b = 0);
    }
}

/// What [`read_hidden_line`] restores when a termination signal comes while echo is off: the
/// terminal's descriptor (`-1`: nothing to restore) and its settings from before. Read by
/// [`on_termination`], a signal handler, so plain statics: the settings are written before the
/// descriptor is published, and only while no handler of this module is installed.
static HIDDEN_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static mut HIDDEN_SAVED: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

/// The signals that end a process by default and that someone else may send while
/// [`read_hidden_line`] waits (with `ISIG` off, the keyboard sends none).
const TERMINATION_SIGNALS: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// [`read_hidden_line`]'s handler of [`TERMINATION_SIGNALS`]: puts the terminal's settings
/// back (`tcsetattr`, async-signal-safe; again when interrupted), then ends the process with
/// the signal as it would have ended without the handler (default action, raised again).
extern "C" fn on_termination(sig: libc::c_int) {
    let fd = HIDDEN_FD.load(std::sync::atomic::Ordering::Acquire);
    // SAFETY: only async-signal-safe calls; `HIDDEN_SAVED` was written before `HIDDEN_FD`
    // was published and is not written while it is.
    unsafe {
        if fd >= 0 {
            let saved = std::ptr::addr_of!(HIDDEN_SAVED).cast::<libc::termios>();
            while libc::tcsetattr(fd, libc::TCSAFLUSH, saved) != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// [`on_termination`] installed for [`TERMINATION_SIGNALS`] while it lives (the handlers from
/// before put back when it drops).
struct TerminationGuard {
    old: Vec<(libc::c_int, libc::sigaction)>,
}

impl TerminationGuard {
    /// Installs the handlers, to restore `saved` on terminal `fd`.
    fn install(fd: std::os::fd::RawFd, saved: &libc::termios) -> TerminationGuard {
        // SAFETY: no handler of this module is installed (one `read_hidden_line` at a time:
        // the CLI asks once), so nothing reads `HIDDEN_SAVED` while it is written; sigaction
        // with a zeroed struct (a valid empty mask) and a handler of the right type.
        unsafe {
            std::ptr::addr_of_mut!(HIDDEN_SAVED)
                .cast::<libc::termios>()
                .write(*saved);
            HIDDEN_FD.store(fd, std::sync::atomic::Ordering::Release);
            let mut old = Vec::new();
            for sig in TERMINATION_SIGNALS {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_termination as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut before: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &action, &mut before) == 0 {
                    old.push((sig, before));
                }
            }
            TerminationGuard { old }
        }
    }
}

impl Drop for TerminationGuard {
    fn drop(&mut self) {
        // SAFETY: the actions sigaction gave back, put back as they were.
        unsafe {
            for (sig, before) in &self.old {
                libc::sigaction(*sig, before, std::ptr::null_mut());
            }
        }
        HIDDEN_FD.store(-1, std::sync::atomic::Ordering::Release);
    }
}

/// Reads a line typed on the terminal without showing it (a token): opens `/dev/tty`, turns
/// echo, line editing and the terminal's signals off (`ECHO`, `ICANON`, `ISIG`, `IEXTEN`;
/// `TCSAFLUSH`, so what was typed before is dropped rather than taken as the line), and only
/// then writes `prompt` there. The bytes are read as [`HiddenLine`] says; with `ISIG` off,
/// Ctrl-C and Ctrl-Z arrive as bytes, so no signal stops the process with echo off. The
/// terminal's settings are restored on every path (again when interrupted), and a newline
/// written; a termination signal sent meanwhile (SIGINT, SIGTERM, SIGHUP, SIGQUIT) restores
/// them before it ends the process ([`on_termination`]). An error when there is no terminal,
/// its settings can't be changed (nothing is read then), or the line is cancelled.
pub fn read_hidden_line(prompt: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
        .map_err(|e| {
            anyhow::anyhow!("cannot open the terminal (/dev/tty: {e}): pipe it in instead")
        })?;
    let fd = tty.as_raw_fd();
    // SAFETY: `termios` is plain data; tcgetattr fills it on success, checked.
    let saved = unsafe {
        let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(fd, t.as_mut_ptr()) != 0 {
            anyhow::bail!(
                "cannot read the terminal's settings ({}): pipe it in instead",
                std::io::Error::last_os_error()
            );
        }
        t.assume_init()
    };
    /// Puts the terminal's settings back when dropped.
    struct Restore {
        fd: std::os::fd::RawFd,
        saved: libc::termios,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: a valid descriptor (the tty, still open) and settings tcgetattr gave.
            unsafe {
                while libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.saved) != 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
            }
        }
    }
    // (Dropped after `restore`: a signal until then restores the settings too.)
    let _signals = TerminationGuard::install(fd, &saved);
    let restore = Restore { fd, saved };
    let mut quiet = saved;
    quiet.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
    quiet.c_cc[libc::VMIN] = 1;
    quiet.c_cc[libc::VTIME] = 0;
    // SAFETY: a valid descriptor and settings derived from tcgetattr's.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) } != 0 {
        anyhow::bail!(
            "cannot turn the terminal's echo off ({}): nothing was read",
            std::io::Error::last_os_error()
        );
    }
    tty.write_all(clean(prompt).as_bytes())?;
    tty.flush()?;
    let mut line = HiddenLine::default();
    let mut chunk = [0u8; 256];
    let outcome = 'read: loop {
        let n = match tty.read(&mut chunk) {
            Ok(0) => break 'read Err(anyhow::anyhow!("the terminal closed")),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break 'read Err(anyhow::anyhow!("reading the terminal: {e}")),
        };
        for &b in &chunk[..n] {
            match line.push(b) {
                Ok(Typed::More) => {}
                Ok(Typed::Done) => break 'read Ok(line.text()),
                Ok(Typed::Cancelled) => break 'read Err(anyhow::anyhow!("cancelled")),
                Err(e) => break 'read Err(e),
            }
        }
    };
    chunk.iter_mut().for_each(|b| *b = 0);
    drop(restore);
    let _ = tty.write_all(b"\n");
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes `input` makes, as [`read_hidden_line`] reads them.
    fn typed(input: &[u8]) -> (anyhow::Result<Typed>, String) {
        let mut line = HiddenLine::default();
        for &b in input {
            match line.push(b) {
                Ok(Typed::More) => {}
                r => return (r, line.text()),
            }
        }
        (Ok(Typed::More), line.text())
    }

    #[test]
    fn hidden_lines_edit_like_a_terminal() {
        let done = |input: &[u8], want: &str| {
            let (r, text) = typed(input);
            assert_eq!(r.unwrap(), Typed::Done, "{input:?}");
            assert_eq!(text, want, "{input:?}");
        };
        done(b"s3cret\n", "s3cret");
        done(b"s3cret\rignored", "s3cret");
        done(b"s3cx\x7fret\n", "s3cret");
        done(b"s3cx\x08ret\n", "s3cret");
        done(b"wrong\x15s3cret\n", "s3cret");
        // Arrow keys, a bracketed paste's markers, SS3 keys: dropped whole.
        done(b"s3\x1b[Dc\x1b[200~ret\x1b[201~\x1bOA\n", "s3cret");
        // Other control bytes (a tab, Ctrl-Z, Ctrl-D in a line): dropped.
        done(b"s3\tc\x1ar\x04et\n", "s3cret");
        assert_eq!(typed(b"s3cr\x03et\n").0.unwrap(), Typed::Cancelled);
        assert_eq!(typed(b"\x04").0.unwrap(), Typed::Cancelled);
        assert_eq!(typed(b"\x1b[\x03").0.unwrap(), Typed::Cancelled);
        let long = vec![b'x'; HIDDEN_LINE_MAX];
        let (r, text) = typed(&[long.as_slice(), b"\n"].concat());
        assert_eq!(r.unwrap(), Typed::Done);
        assert_eq!(text.len(), HIDDEN_LINE_MAX);
        let (r, _) = typed(&[long.as_slice(), b"y"].concat());
        assert!(r.unwrap_err().to_string().contains("longer than 1024"));
    }

    #[test]
    fn controls_are_escaped_and_text_kept() {
        assert!(matches!(clean("plain\ttext\nok ü"), Cow::Borrowed(_)));
        assert_eq!(
            clean("a\u{1b}]0;x\u{7}b\u{1b}[2J\r\u{7f}\u{9b}31m\u{0}"),
            "a\\u001b]0;x\\u0007b\\u001b[2J\\u000d\\u007f\\u009b31m\\u0000"
        );
        // JSON stays JSON, with the same meaning.
        let j = serde_json::json!({ "s": "x\u{9b}y\u{1b}" }).to_string();
        let back: serde_json::Value = serde_json::from_str(&clean(&j)).unwrap();
        assert_eq!(back["s"], "x\u{9b}y\u{1b}");
    }
}

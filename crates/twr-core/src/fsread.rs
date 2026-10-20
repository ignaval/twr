//! Reading another program's settings file (a runtime's `settings.json` or `config.toml`,
//! Claude Code's managed settings) or git's small metadata files (a worktree's `.git` file,
//! a git directory's `commondir`) where twr must neither wait nor allocate without bound:
//! the hub reads them under its core lock (`account.list`, a spawn, an attach), so a FIFO
//! put in such a file's place must not stall the hub, nor `/dev/zero` fill its memory.

use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

/// The largest settings file [`read_settings`] reads: 1 MiB.
pub const SETTINGS_MAX: u64 = 1 << 20;

/// The largest git metadata file [`read_git_file`] reads: 8 KiB, room for a `gitdir: `
/// line naming a path as long as Linux allows (`PATH_MAX`, 4 KiB).
pub const GIT_FILE_MAX: u64 = 8 << 10;

/// The text of settings file `path`, its links followed: `None` when there is none (nothing
/// there, or a link to nothing). Read without blocking and within [`SETTINGS_MAX`]: the path
/// is resolved first, then opened with `O_NOFOLLOW | O_NONBLOCK` (a link swapped in since
/// fails to open; a FIFO opens without waiting for a writer), and only a regular file of at
/// most [`SETTINGS_MAX`] bytes is read. Anything else is an error that says what it is (not
/// a regular file, too large, not UTF-8, or why it can't be opened): the caller can't tell
/// what the file sets.
pub fn read_settings(path: &Path) -> io::Result<Option<String>> {
    read_bounded(path, SETTINGS_MAX)
}

/// The text of git metadata file `path` (a worktree's `.git` file, a git directory's
/// `commondir`, a worktree record's `gitdir`), read as [`read_settings`] reads but within
/// [`GIT_FILE_MAX`]: such files are one short line, and the checkout they are in may be an
/// agent's to write.
pub fn read_git_file(path: &Path) -> io::Result<Option<String>> {
    read_bounded(path, GIT_FILE_MAX)
}

/// [`read_settings`] with a cap of `max` bytes.
fn read_bounded(path: &Path, max: u64) -> io::Result<Option<String>> {
    let real = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&real)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(invalid(
                "it was replaced by a symbolic link while it was read",
            ))
        }
        Err(e) => return Err(e),
    };
    let meta = f.metadata()?;
    if !meta.is_file() {
        let t = meta.file_type();
        let what = if t.is_dir() {
            "a directory"
        } else if t.is_fifo() {
            "a FIFO"
        } else if t.is_char_device() {
            "a character device"
        } else if t.is_block_device() {
            "a block device"
        } else if t.is_socket() {
            "a socket"
        } else {
            "something else"
        };
        return Err(invalid(format!("it is {what}, not a regular file")));
    }
    let too_large = || invalid(format!("it is larger than {} KiB", max >> 10));
    if meta.len() > max {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_large());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("it is not UTF-8 text"))
}

fn invalid(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn mkfifo(p: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// A regular file is read through its links; nothing there is `None`; a FIFO (no writer:
    /// a blocking open would wait forever), a device, a directory and a file over the cap are
    /// refused at once, each saying what it is.
    #[test]
    fn settings_reads_are_bounded_and_never_block() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("settings.json");
        std::fs::write(&file, "{}").unwrap();
        let link = t.path().join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_settings(&file).unwrap().as_deref(), Some("{}"));
        assert_eq!(read_settings(&link).unwrap().as_deref(), Some("{}"));
        assert_eq!(read_settings(&t.path().join("gone")).unwrap(), None);
        let dangling = t.path().join("dangling.json");
        std::os::unix::fs::symlink(t.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(read_settings(&dangling).unwrap(), None);

        let fifo = t.path().join("fifo.json");
        mkfifo(&fifo);
        let fifo_link = t.path().join("fifo-link.json");
        std::os::unix::fs::symlink(&fifo, &fifo_link).unwrap();
        let big = t.path().join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(SETTINGS_MAX + 1).unwrap();
        let exact = t.path().join("exact.json");
        std::fs::write(&exact, vec![b' '; SETTINGS_MAX as usize]).unwrap();
        assert_eq!(
            read_settings(&exact).unwrap().map(|s| s.len()),
            Some(SETTINGS_MAX as usize)
        );
        let start = Instant::now();
        for (p, why) in [
            (&fifo, "it is a FIFO, not a regular file"),
            (&fifo_link, "it is a FIFO, not a regular file"),
            (&big, "it is larger than 1024 KiB"),
            (
                &Path::new("/dev/zero").to_path_buf(),
                "it is a character device, not a regular file",
            ),
            (
                &t.path().to_path_buf(),
                "it is a directory, not a regular file",
            ),
        ] {
            let e = read_settings(p).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{}", p.display());
            assert_eq!(e.to_string(), why, "{}", p.display());
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        assert_eq!(
            read_settings(&file).unwrap_err().to_string(),
            "it is not UTF-8 text"
        );
    }

    /// A git metadata file is read as a settings file is, within [`GIT_FILE_MAX`]: a FIFO in
    /// its place is refused at once (no writer: a blocking open would wait forever), as is a
    /// file over the cap. Run on a thread of its own, so a read that blocks fails the test
    /// instead of hanging it.
    #[test]
    fn git_file_reads_are_bounded_and_never_block() {
        let t = tempfile::tempdir().unwrap();
        let dot_git = t.path().join(".git");
        std::fs::write(&dot_git, "gitdir: /r/.git/worktrees/w\n").unwrap();
        let fifo = t.path().join("commondir");
        mkfifo(&fifo);
        let exact = t.path().join("exact");
        std::fs::write(&exact, vec![b'a'; GIT_FILE_MAX as usize]).unwrap();
        let big = t.path().join("big");
        std::fs::write(&big, vec![b'a'; GIT_FILE_MAX as usize + 1]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let read = |p: &Path| read_git_file(p).map_err(|e| e.to_string());
            let _ = tx.send((
                read(&dot_git),
                read(&fifo),
                read(&exact).map(|s| s.map(|s| s.len())),
                read(&big),
                read(&t.path().join("gone")),
            ));
        });
        let (dot_git, fifo, exact, big, gone) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a git file read blocked");
        assert_eq!(dot_git, Ok(Some("gitdir: /r/.git/worktrees/w\n".into())));
        assert_eq!(fifo, Err("it is a FIFO, not a regular file".into()));
        assert_eq!(exact, Ok(Some(GIT_FILE_MAX as usize)));
        assert_eq!(big, Err("it is larger than 8 KiB".into()));
        assert_eq!(gone, Ok(None));
    }
}

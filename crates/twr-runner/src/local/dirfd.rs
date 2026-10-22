//! Directories held open, and changes made relative to them (`openat`, `unlinkat`, `renameat`,
//! `renameat2` or `renameatx_np`, `mkdirat`, `fchmodat`): each step names one entry of a
//! directory already held and never resolves a path again, so a symlink met on the way is never
//! followed. What deletes a worktree's build output uses them
//! ([`super::trash::drop_build_output`]), and so does the hub's write of a project's standing
//! context (`twr-hub`'s `hub::context`).
//!
//! That protects against what is there when the deletion runs. It does not protect against
//! another process writing into the worktree meanwhile (twr has stopped the thread's runtime
//! by then): a parent of the directory it starts from swapped for a symlink (that directory is
//! opened by path), or a repository made while it deletes. A mount point is no symlink: the
//! deletion runs only on Linux and macOS, where a worktree with one in it is not deleted at all
//! (`trash::mounts_inside`), and it stops at a directory on another file system. (Off Linux a
//! directory is listed by the path it was reached by, [`Dir::names`]; [`Dir::listed`] lists one
//! through its descriptor everywhere, for a clone's cleanup, `crate::repos`.)
//!
//! The calls are declared here, and their flags spelled out per OS and architecture (as
//! `twr-hub`'s `hub::context` does). Where they are not, [`Dir::open`] fails and nothing is
//! changed this way. The listing through a descriptor ([`Dir::listed`]) takes `fdopendir` and
//! `readdir` from `libc` instead: their `dirent` differs per system, and so do their symbols
//! (`$INODE64` on an Intel Mac).

use std::ffi::{c_char, c_int, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

extern "C" {
    fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int;
    fn renameat(
        olddirfd: c_int,
        oldpath: *const c_char,
        newdirfd: c_int,
        newpath: *const c_char,
    ) -> c_int;
    fn mkdirat(dirfd: c_int, path: *const c_char, mode: Mode) -> c_int;
    fn fchmodat(dirfd: c_int, path: *const c_char, mode: Mode, flags: c_int) -> c_int;
}

// `renameat2` through `syscall`: the C library may not have it (musl before 1.2.5).
#[cfg(target_os = "linux")]
extern "C" {
    fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
}

#[cfg(target_os = "macos")]
extern "C" {
    fn renameatx_np(
        fromfd: c_int,
        from: *const c_char,
        tofd: c_int,
        to: *const c_char,
        flags: std::ffi::c_uint,
    ) -> c_int;
}

/// `mode_t`.
#[cfg(target_os = "linux")]
type Mode = u32;
#[cfg(not(target_os = "linux"))]
type Mode = u16;

/// The flags of the calls, and `ELOOP`.
struct Sys {
    /// `O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC` (and `O_RDONLY`, 0).
    open_dir: c_int,
    /// `O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK` (a FIFO does not block the open).
    open_file: c_int,
    /// `O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`: a new file, never what is there.
    create_file: c_int,
    /// `AT_REMOVEDIR`.
    remove_dir: c_int,
    /// `AT_SYMLINK_NOFOLLOW`.
    no_follow: c_int,
    /// `errno` of an `O_NOFOLLOW` open of a symlink.
    eloop: i32,
}

/// Linux's `O_NOFOLLOW` and `O_DIRECTORY` are `0o400000` and `0o200000` in the generic ABI,
/// x86 included, but `0o100000` and `0o40000` on ARM.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
static SYS: Option<Sys> = Some(Sys {
    open_dir: 0o400000 | 0o200000 | 0o2000000,
    open_file: 0o400000 | 0o2000000 | 0o4000,
    create_file: 0o1 | 0o100 | 0o200 | 0o400000 | 0o2000000,
    remove_dir: 0x200,
    no_follow: 0x100,
    eloop: 40,
});
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
static SYS: Option<Sys> = Some(Sys {
    open_dir: 0o100000 | 0o40000 | 0o2000000,
    open_file: 0o100000 | 0o2000000 | 0o4000,
    create_file: 0o1 | 0o100 | 0o200 | 0o100000 | 0o2000000,
    remove_dir: 0x200,
    no_follow: 0x100,
    eloop: 40,
});
#[cfg(target_os = "macos")]
static SYS: Option<Sys> = Some(Sys {
    open_dir: 0x0100 | 0x100000 | 0x1000000,
    open_file: 0x0100 | 0x1000000 | 0x0004,
    create_file: 0x0001 | 0x0200 | 0x0800 | 0x0100 | 0x1000000,
    remove_dir: 0x0080,
    no_follow: 0x0020,
    eloop: 62,
});
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "arm",
            target_arch = "aarch64"
        )
    )
)))]
static SYS: Option<Sys> = None;

/// [`SYS`], or an error where there is none.
fn sys() -> io::Result<&'static Sys> {
    SYS.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "no descriptor-relative file operations on this system",
        )
    })
}

/// `name` for a call: one entry, never a path.
fn entry(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not the name of an entry"),
        ));
    }
    CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// A call's result.
fn done(r: c_int) -> io::Result<c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Whether open error `e` says the entry is no directory (a symlink, which is not followed, or
/// a file).
pub fn not_a_dir(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotADirectory
        || sys().is_ok_and(|s| e.raw_os_error() == Some(s.eloop))
}

/// A directory held open, with the path it was reached by (for messages, and for listing it
/// where there is no other way, [`Dir::names`]).
#[derive(Debug)]
pub struct Dir {
    file: File,
    path: PathBuf,
}

impl Dir {
    /// The directory at `path`, which is not a symlink (a symlink on the way to it is
    /// followed).
    pub fn open(path: &Path) -> io::Result<Dir> {
        let file = File::options()
            .read(true)
            .custom_flags(sys()?.open_dir)
            .open(path)?;
        Ok(Dir {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Its entry `name`, a directory (a symlink is not followed: an error).
    pub fn dir(&self, name: &OsStr) -> io::Result<Dir> {
        let c = entry(name)?;
        // SAFETY: `c` is a NUL-terminated name that outlives the call; the descriptor it
        // returns is owned by the `File` alone.
        let fd = done(unsafe { openat(self.file.as_raw_fd(), c.as_ptr(), sys()?.open_dir) })?;
        Ok(Dir {
            // SAFETY: `fd` was just opened, and nothing else owns it.
            file: unsafe { File::from_raw_fd(fd) },
            path: self.path.join(name),
        })
    }

    /// Removes its entry `name`, anything but a directory (a symlink itself).
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let c = entry(name)?;
        // SAFETY: as in `dir`.
        done(unsafe { unlinkat(self.file.as_raw_fd(), c.as_ptr(), 0) }).map(drop)
    }

    /// Removes its entry `name`, an empty directory.
    pub(crate) fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        let c = entry(name)?;
        // SAFETY: as in `dir`.
        done(unsafe { unlinkat(self.file.as_raw_fd(), c.as_ptr(), sys()?.remove_dir) }).map(drop)
    }

    /// Moves its entry `name` (whatever it is) to `to`'s entry `new` only when there is none:
    /// `AlreadyExists` otherwise, and an error where that cannot be told atomically
    /// ([`rename_new`]).
    pub fn rename_new(&self, name: &OsStr, to: &Dir, new: &OsStr) -> io::Result<()> {
        let (a, b) = (entry(name)?, entry(new)?);
        rename_new(self.file.as_raw_fd(), &a, to.file.as_raw_fd(), &b)
    }

    /// Makes the directory `name` in it with `mode` (less the umask).
    pub fn make_dir(&self, name: &OsStr, mode: u16) -> io::Result<()> {
        let c = entry(name)?;
        // SAFETY: as in `dir`.
        done(unsafe { mkdirat(self.file.as_raw_fd(), c.as_ptr(), Mode::from(mode)) }).map(drop)
    }

    /// Makes the file `name` in it holding `bytes` (synced, for its owner only), only when there
    /// is nothing of that name: `AlreadyExists` otherwise (a symlink there too, never followed).
    pub(super) fn write_new(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = self.create_new(name, 0o600)?;
        f.write_all(bytes)?;
        f.sync_all()
    }

    /// Makes the empty file `name` in it, open for writing, with `mode` (less the umask), only
    /// when there is nothing of that name: `AlreadyExists` otherwise (a symlink there too,
    /// never followed).
    pub fn create_new(&self, name: &OsStr, mode: u16) -> io::Result<File> {
        let c = entry(name)?;
        let mode = std::ffi::c_uint::from(mode);
        // SAFETY: as in `dir`; the mode is passed as the `unsigned int` `openat` reads.
        let fd =
            done(unsafe { openat(self.file.as_raw_fd(), c.as_ptr(), sys()?.create_file, mode) })?;
        // SAFETY: `fd` was just opened, and nothing else owns it.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Renames its entry `name` to its entry `new`, replacing what is there (a symlink itself,
    /// never what it names).
    pub fn rename(&self, name: &OsStr, new: &OsStr) -> io::Result<()> {
        let (a, b) = (entry(name)?, entry(new)?);
        let fd = self.file.as_raw_fd();
        // SAFETY: as in `dir`.
        done(unsafe { renameat(fd, a.as_ptr(), fd, b.as_ptr()) }).map(drop)
    }

    /// Its entry `name` opened for reading, whatever it is but a symlink (not followed: an
    /// error, [`not_a_dir`] says so), without waiting on a FIFO.
    pub fn open_entry(&self, name: &OsStr) -> io::Result<File> {
        let c = entry(name)?;
        // SAFETY: as in `dir`.
        let fd = done(unsafe { openat(self.file.as_raw_fd(), c.as_ptr(), sys()?.open_file) })?;
        // SAFETY: `fd` was just opened, and nothing else owns it.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Replaces its entry `name` (whatever it is: a symlink is replaced, not followed) with a
    /// file holding `bytes`, written whole to a new file of its own first ([`Dir::write_new`],
    /// `<name>.tmp`, else the first free `<name>.tmp.<n>`) and renamed over it.
    pub(super) fn replace(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        for n in 0..100 {
            let mut tmp = name.to_os_string();
            tmp.push(".tmp");
            if n > 0 {
                tmp.push(format!(".{n}"));
            }
            match self.write_new(&tmp, bytes) {
                Ok(()) => {
                    let renamed = self.rename(&tmp, name);
                    if renamed.is_err() {
                        let _ = self.unlink(&tmp);
                    }
                    return renamed;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free name for a new {:?}", self.path.join(name)),
        ))
    }

    /// The contents of its entry `name`, a regular file of at most `max` bytes (a symlink is
    /// not followed, and nothing else is read: an error).
    pub(super) fn read_small(&self, name: &OsStr, max: u64) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let f = self.open_entry(name)?;
        if !f.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", self.path.join(name).display()),
            ));
        }
        let mut out = Vec::new();
        f.take(max + 1).read_to_end(&mut out)?;
        if out.len() as u64 > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is over {max} bytes", self.path.join(name).display()),
            ));
        }
        Ok(out)
    }

    /// Makes its entry `name` readable, writable and searchable by its owner alone (a symlink
    /// is not followed: an error).
    pub(crate) fn open_up(&self, name: &OsStr) -> io::Result<()> {
        let c = entry(name)?;
        // SAFETY: as in `dir`.
        done(unsafe { fchmodat(self.file.as_raw_fd(), c.as_ptr(), 0o700, sys()?.no_follow) })
            .map(drop)
    }

    /// The names of its entries.
    pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
        self.entries()?.collect()
    }

    /// The names of its entries, read from the system as they are taken (a buffer at a
    /// time): a directory of any size is never read whole before its first name is used, so
    /// a walk with a budget ([`super::trash::worktree_disk`]) stops reading where it stops.
    pub(super) fn entries(&self) -> io::Result<impl Iterator<Item = io::Result<OsString>>> {
        Ok(std::fs::read_dir(self.path())?.map(|e| {
            #[cfg(test)]
            LISTED.with(|n| n.set(n.get() + 1));
            e.map(|e| e.file_name())
        }))
    }

    /// The names of its entries (`.` and `..` left out), read through its descriptor on every
    /// system: `fdopendir` of a duplicate of it, rewound first (a duplicate shares its read
    /// position). Never by a path: the directory held, wherever it is now and whatever is at the
    /// path it was reached by.
    pub fn listed(&self) -> io::Result<Vec<OsString>> {
        sys()?;
        /// Closes the stream (and with it the duplicate) when dropped.
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                // SAFETY: a stream `fdopendir` opened, closed once.
                unsafe { libc::closedir(self.0) };
            }
        }
        // SAFETY: fcntl(2) duplicating a descriptor this `Dir` holds open; the duplicate is
        // owned here (closed below, or by the stream made of it).
        let fd = done(unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
        // SAFETY: `fd` is open and a directory's; on success the stream owns it.
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: `fd` is still this function's (fdopendir failed).
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let stream = Stream(dir);
        // SAFETY: a stream just opened.
        unsafe { libc::rewinddir(stream.0) };
        let mut names = Vec::new();
        loop {
            // `readdir` says an error only through `errno`.
            set_errno(0);
            // SAFETY: a stream open until `stream` is dropped; the entry it returns is read
            // before the next call.
            let ent = unsafe { libc::readdir(stream.0) };
            if ent.is_null() {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(0) | None => Ok(names),
                    Some(_) => Err(e),
                };
            }
            // SAFETY: `d_name` of the entry `readdir` returned is NUL-terminated.
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_os_string());
            }
        }
    }

    /// A path to it. On Linux through its descriptor (`/proc/self/fd/<n>`: the directory held,
    /// wherever it is now); elsewhere the path it was reached by, which a listing reads only
    /// names from (every change is made relative to the directory held, and one emptied of only
    /// what was listed is not removed).
    pub(super) fn path(&self) -> PathBuf {
        if cfg!(target_os = "linux") {
            PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        } else {
            self.path.clone()
        }
    }

    /// The path it was reached by.
    pub(crate) fn shown(&self) -> &Path {
        &self.path
    }

    /// The directory, open.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Sets this thread's `errno`.
fn set_errno(v: c_int) {
    #[cfg(target_os = "linux")]
    // SAFETY: the calling thread's `errno`, always a valid location.
    unsafe {
        *libc::__errno_location() = v;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: as above.
    unsafe {
        *libc::__error() = v;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = v;
}

#[cfg(test)]
thread_local! {
    /// How many names [`Dir::entries`] has handed out on this thread.
    pub(super) static LISTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// `renameat2(.., RENAME_NOREPLACE)` (Linux 3.15; a file system that does not take it fails
/// with `EINVAL`), by its number per architecture.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
fn rename_new(from: c_int, a: &CString, to: c_int, b: &CString) -> io::Result<()> {
    use std::ffi::c_long;
    #[cfg(target_arch = "x86_64")]
    const RENAMEAT2: c_long = 316;
    #[cfg(target_arch = "x86")]
    const RENAMEAT2: c_long = 353;
    #[cfg(target_arch = "arm")]
    const RENAMEAT2: c_long = 382;
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    const RENAMEAT2: c_long = 276;
    const RENAME_NOREPLACE: c_long = 1;
    // SAFETY: the names are NUL-terminated and outlive the call; every argument is passed as
    // the `long` the kernel reads.
    let r = unsafe {
        syscall(
            RENAMEAT2,
            c_long::from(from),
            a.as_ptr(),
            c_long::from(to),
            b.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `renameatx_np(.., RENAME_EXCL)` (macOS 10.12).
#[cfg(target_os = "macos")]
fn rename_new(from: c_int, a: &CString, to: c_int, b: &CString) -> io::Result<()> {
    const RENAME_EXCL: std::ffi::c_uint = 0x0000_0004;
    // SAFETY: as in `Dir::dir`.
    done(unsafe { renameatx_np(from, a.as_ptr(), to, b.as_ptr(), RENAME_EXCL) }).map(drop)
}

#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "loongarch64"
        )
    )
)))]
fn rename_new(_: c_int, _: &CString, _: c_int, _: &CString) -> io::Result<()> {
    sys().map(drop)?;
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`Dir::listed`] lists the directory held through its descriptor: the same names every
    /// time (a listing does not leave the next one part way), and the directory's own names
    /// after it was renamed and another one put at its path.
    #[test]
    fn a_held_directory_is_listed_through_its_descriptor() {
        let d = tempfile::tempdir().unwrap();
        let at = d.path().join("held");
        std::fs::create_dir(&at).unwrap();
        for n in ["a", "b", ".git"] {
            std::fs::write(at.join(n), "").unwrap();
        }
        let dir = Dir::open(&at).unwrap();
        let sorted = |mut v: Vec<OsString>| {
            v.sort();
            v
        };
        let want: Vec<OsString> = [".git", "a", "b"].map(OsString::from).to_vec();
        assert_eq!(sorted(dir.listed().unwrap()), want);
        assert_eq!(sorted(dir.listed().unwrap()), want);
        std::fs::rename(&at, d.path().join("moved")).unwrap();
        std::fs::create_dir(&at).unwrap();
        std::fs::write(at.join("other"), "").unwrap();
        assert_eq!(sorted(dir.listed().unwrap()), want);
        let file = dir.dir(OsStr::new(".git")).unwrap_err();
        assert!(not_a_dir(&file), "{file}");
    }
}

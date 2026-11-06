use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
pub const MAX_FILE: usize = 25 * 1024 * 1024;
pub struct Upload {
    base: File,
    directory: String,
    pub created: std::time::Instant,
    file: File,
    dir: File,
    name: String,
    pub path: PathBuf,
    size: usize,
    written: usize,
    hash: Sha256,
    expected: String,
    committed: bool,
}
fn dir_at(parent: &File, name: &str) -> Result<File> {
    let name = CString::new(name)?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    anyhow::ensure!(
        fd >= 0,
        "Unsafe upload directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Upload {
    pub fn begin(root: &Path, name: &str, size: usize, expected: &str) -> Result<Self> {
        anyhow::ensure!(size <= MAX_FILE, "Files can be at most 25 MiB");
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 120
                && name != "."
                && name != ".."
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_. -".contains(c)),
            "Use a filename without directories or control characters"
        );
        anyhow::ensure!(
            expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
            "File checksum required"
        );
        let root_file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .context("Opening thread working directory")?;
        let base = dir_at(&root_file, ".twr-uploads")?;
        let id = uuid::Uuid::new_v4().to_string();
        let dir = dir_at(&base, &id)?;
        let part_name = CString::new(format!("{name}.part"))?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                part_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        anyhow::ensure!(fd >= 0, "Could not create staged upload");
        let path = root.join(".twr-uploads").join(&id).join(name);
        Ok(Self {
            base,
            directory: id,
            created: std::time::Instant::now(),
            file: unsafe { File::from_raw_fd(fd) },
            dir,
            name: name.to_owned(),
            path,
            size,
            written: 0,
            hash: Sha256::new(),
            expected: expected.to_lowercase(),
            committed: false,
        })
    }
    pub fn chunk(&mut self, data: &[u8]) -> Result<usize> {
        anyhow::ensure!(
            data.len() <= 1024 * 1024 && self.written + data.len() <= self.size,
            "Upload exceeds its declared size"
        );
        self.file.write_all(data)?;
        self.hash.update(data);
        self.written += data.len();
        Ok(self.written)
    }
    pub fn commit(mut self) -> Result<PathBuf> {
        anyhow::ensure!(self.written == self.size, "Upload is incomplete");
        anyhow::ensure!(
            hex::encode(self.hash.clone().finalize()) == self.expected,
            "Upload checksum mismatch"
        );
        self.file.sync_all()?;
        let part = CString::new(format!("{}.part", self.name))?;
        let name = CString::new(self.name.as_str())?;
        let result = unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                part.as_ptr(),
                self.dir.as_raw_fd(),
                name.as_ptr(),
            )
        };
        anyhow::ensure!(
            result == 0,
            "Could not commit upload: {}",
            std::io::Error::last_os_error()
        );
        self.committed = true;
        Ok(self.path.clone())
    }
}
impl Drop for Upload {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(name) = CString::new(format!("{}.part", self.name)) {
                unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) };
            }
            if let Ok(name) = CString::new(self.directory.as_str()) {
                unsafe { libc::unlinkat(self.base.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_file_checks_size_hash_and_traversal() {
        let root = tempfile::tempdir().unwrap();
        let hash = hex::encode(Sha256::digest(b"hello"));
        assert!(Upload::begin(root.path(), "../escape", 5, &hash).is_err());
        let mut file = Upload::begin(root.path(), "hello.txt", 5, &hash).unwrap();
        assert!(file.chunk(b"oversize").is_err());
        file.chunk(b"hello").unwrap();
        let path = file.commit().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
    }
    #[test]
    fn symlink_destination_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".twr-uploads")).unwrap();
        assert!(Upload::begin(root.path(), "x", 0, &hex::encode(Sha256::digest(b""))).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

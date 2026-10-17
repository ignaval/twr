//! Helpers shared by the crate's unit tests.

use std::path::Path;

/// Creates the executable `path` (mode 755, its directories made) holding `content`, without
/// this process ever holding it open for writing: the bytes go to a side file, `cp` writes
/// `path`, and the side file is removed. A child another test thread forks while this process
/// writes the file would inherit the descriptor, and exec'ing the file would fail with
/// ETXTBSY until that child execs or exits.
pub(crate) fn install(path: &Path, content: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

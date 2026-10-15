//! `twr context edit` never loses an edit: the hub is reconnected after the
//! editor exits, the set carries the hash that was read (a concurrent change is a conflict),
//! and on any failure after the editor the temporary copy is kept and its path printed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const TWR: &str = env!("CARGO_BIN_EXE_twr");

/// The `twr` binary, without a `TWR_WORKSPACE` the test's own environment may have (the tests
/// here run in the `default` workspace of their `TWR_HOME`).
fn twr_cmd() -> Command {
    let mut c = Command::new(TWR);
    c.env_remove("TWR_WORKSPACE");
    c.env("TWR_TEST_USER_TOKEN_FILE", test_token());
    c
}

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`): run
/// from inside a twr thread, the tests' clients would otherwise count as that thread's agent
/// to the hubs they start, which descend from it too.
fn test_token() -> &'static std::path::Path {
    static TOKEN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Private to this user, or the hub ignores it.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!("{nanos:x}{:x}", std::process::id()).as_bytes())
            .unwrap();
        path
    })
}
const WAIT: Duration = Duration::from_secs(15);

fn cmd(home: &Path) -> Command {
    let mut c = twr_cmd();
    c.env("TWR_HOME", home)
        .env("TMPDIR", home.join("tmp"))
        .env("TWR_ENABLE_FAKE", "1")
        .env_remove("RUST_LOG")
        .env_remove("VISUAL");
    c
}

fn twr(home: &Path, args: &[&str]) -> Output {
    cmd(home).args(args).output().expect("running twr")
}

fn poll(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct HubProc(Child);

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_hub(home: &Path) -> HubProc {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("hub-test.log"))
        .unwrap();
    let hub = HubProc(
        cmd(home)
            .arg("hub")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub socket", || {
        twr(home, &["thread", "list"]).status.success()
    });
    hub
}

/// A home with a hub and a thread of project `proj`; returns the hub and the context file.
fn setup(home: &Path) -> (HubProc, PathBuf) {
    std::fs::create_dir(home.join("tmp")).unwrap();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let hub = start_hub(home);
    let out = twr(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    (hub, project.join(".twr").join("context.md"))
}

/// The path after "kept in " in `stderr`.
fn kept_path(stderr: &str) -> PathBuf {
    let rest = &stderr[stderr.find("kept in ").expect(stderr) + "kept in ".len()..];
    let path = PathBuf::from(
        rest.split([' ', '\n'])
            .next()
            .unwrap()
            .trim_end_matches([':', ')']),
    );
    assert!(path.exists(), "{stderr}");
    path
}

fn temp_files(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join("tmp"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

#[test]
fn a_concurrent_change_is_a_conflict_and_the_edit_is_kept() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let (_hub, file) = setup(home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "v1\n").unwrap();

    // While the editor is open, someone else changes the file.
    let script = home.join("editor.sh");
    std::fs::write(
        &script,
        format!(
            "printf 'v2 by hand\\n' > '{}'\nprintf 'my edit\\n' >> \"$1\"\n",
            file.display()
        ),
    )
    .unwrap();
    let out = cmd(home)
        .args(["context", "edit", "proj"])
        .env("EDITOR", format!("sh {}", script.display()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("conflict"), "{stderr}");
    let kept = kept_path(&stderr);
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "v1\nmy edit\n");
    // The concurrent change was not overwritten.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2 by hand\n");

    // A clean edit leaves no temporary file behind.
    std::fs::remove_file(&kept).unwrap();
    std::fs::write(&script, "printf 'more\\n' >> \"$1\"\n").unwrap();
    let out = cmd(home)
        .args(["context", "edit", "proj"])
        .env("EDITOR", format!("sh {}", script.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "v2 by hand\nmore\n"
    );
    assert!(temp_files(home).is_empty(), "{:?}", temp_files(home));

    // An editor that fails after writing: nothing saved, the copy is kept.
    std::fs::write(&script, "printf 'draft\\n' >> \"$1\"; exit 1\n").unwrap();
    let out = cmd(home)
        .args(["context", "edit", "proj"])
        .env("EDITOR", format!("sh {}", script.display()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("the context is unchanged"), "{stderr}");
    let kept = kept_path(&stderr);
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "v2 by hand\nmore\ndraft\n"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "v2 by hand\nmore\n"
    );
}

#[test]
fn the_hub_is_reconnected_after_the_editor_and_a_failed_save_keeps_the_edit() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let (hub, file) = setup(home);
    let open = home.join("editor-open");
    let go = home.join("editor-go");
    let script = home.join("editor.sh");
    std::fs::write(
        &script,
        format!(
            "touch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nprintf 'kept rules\\n' >> \"$1\"\n",
            open.display(),
            go.display()
        ),
    )
    .unwrap();
    let edit = |home: &Path| {
        cmd(home)
            .args(["context", "edit", "proj"])
            .env("EDITOR", format!("sh {}", script.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };

    // The hub restarts while the editor is open: the save goes to the new hub.
    let child = edit(home);
    poll("the editor", || open.exists());
    drop(hub);
    let hub = start_hub(home);
    std::fs::write(&go, "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "kept rules\n");

    // The hub is gone when the editor exits: the edit is kept and its path printed.
    std::fs::remove_file(&open).unwrap();
    std::fs::remove_file(&go).unwrap();
    let child = edit(home);
    poll("the editor", || open.exists());
    drop(hub);
    std::fs::write(&go, "").unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let kept = kept_path(&stderr);
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "kept rules\nkept rules\n"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "kept rules\n");
}

#[test]
fn a_non_utf8_context_is_not_edited() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let (_hub, file) = setup(home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"ok \xff\xfe bytes").unwrap();
    let out = cmd(home)
        .args(["context", "edit", "proj"])
        .env("EDITOR", "false")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("not UTF-8"), "{stderr}");
    assert_eq!(std::fs::read(&file).unwrap(), b"ok \xff\xfe bytes");
    assert!(temp_files(home).is_empty());
}

/// An editor that writes, makes its file unreadable and fails must not lose the
/// edit — a read error is never proof that the file is unchanged.
#[test]
fn an_unreadable_edit_after_a_failed_editor_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let (_hub, file) = setup(home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "v1\n").unwrap();
    let script = home.join("editor.sh");
    std::fs::write(
        &script,
        "printf 'precious edit\\n' >> \"$1\"; chmod 000 \"$1\"; exit 1\n",
    )
    .unwrap();
    let out = cmd(home)
        .args(["context", "edit", "proj"])
        .env("EDITOR", format!("sh {}", script.display()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("the context is unchanged"), "{stderr}");
    let kept = kept_path(&stderr);
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "v1\nprecious edit\n"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1\n");
}

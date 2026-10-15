//! `twr __job-gate`: the start gate the hub runs a job's command through (`twr_hub::job_gate`),
//! dispatched before anything else of `twr`'s.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const TWR: &str = env!("CARGO_BIN_EXE_twr");

/// The gate says `ready` once it found the program, runs it only after `go` (with
/// `/dev/null` for input, its environment as given), and runs nothing when its socket closes
/// first; a program it cannot find is `missing`.
#[test]
fn twr_runs_a_jobs_start_gate_first_thing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let gate = |argv: &[&str]| {
        let (hub, gate) = UnixStream::pair().unwrap();
        let child = std::process::Command::new(TWR)
            .arg("__job-gate")
            .arg("--")
            .args(argv)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("odd.name", "kept")
            .current_dir(dir.path())
            .stdin(std::os::fd::OwnedFd::from(gate))
            .stdout(std::fs::File::create(&out).unwrap())
            .spawn()
            .unwrap();
        (hub, child)
    };
    let line = |hub: &UnixStream| {
        let mut l = String::new();
        BufReader::new(hub).read_line(&mut l).unwrap();
        l
    };
    let (mut hub, mut child) = gate(&["env"]);
    assert_eq!(line(&hub), "ready\n");
    hub.write_all(b"go\n").unwrap();
    assert!(child.wait().unwrap().success());
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("odd.name=kept\n"), "{text}");
    #[cfg(target_os = "linux")]
    {
        let (mut hub, mut child) = gate(&["readlink", "/proc/self/fd/0"]);
        assert_eq!(line(&hub), "ready\n");
        hub.write_all(b"go\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "/dev/null\n");
    }

    let (hub, mut child) = gate(&["touch", "ran"]);
    assert_eq!(line(&hub), "ready\n");
    drop(hub);
    assert_eq!(child.wait().unwrap().code(), Some(125));
    assert!(!dir.path().join("ran").exists());

    let (hub, mut child) = gate(&["no-such-program-twr"]);
    assert!(line(&hub).starts_with("missing `no-such-program-twr` not found on PATH"));
    assert_eq!(child.wait().unwrap().code(), Some(127));
}

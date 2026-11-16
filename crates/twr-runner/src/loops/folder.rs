//! A runner's copy of a loop's folder: `<runner data>/loops/<loop id>/`, private
//! (docs/design/loops.md, "Remote boxes"). The hub's own folder is the loop's record; this one
//! is working space for the thread and the reviewer on the box, who read its files by their
//! absolute paths there. The hub writes into it ([`files`], `Op::LoopFiles`), a round diff the
//! box builds is saved into it (`Op::LoopGit`), a review run writes its answer and log into it,
//! and the hub reads those back. A folder the box lost (cleaned, reinstalled) is written again
//! by the hub before the next step reads it: [`files`] says which files it lacks.
//!
//! Every name is checked ([`valid_loop_id`], [`valid_loop_file`]): one plain file in one
//! folder under the loops directory, never a path the hub could aim elsewhere. Writes go
//! through a temporary file and a rename, so a reader never sees half a file.
//!
//! [`forget`] removes a loop's folder (`Op::LoopForget`: the thread was deleted, or a later
//! loop of it started); [`prune`] removes folders untouched for [`KEEP_UNUSED`] when the box
//! opens, for a hub that never said.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use twr_core::runner::{
    cmd_errors, valid_loop_file, valid_loop_id, CmdError, LoopFile, LOOP_FILES_MAX,
};

/// How long a loop folder no hub touched is kept before [`prune`] removes it.
pub const KEEP_UNUSED: Duration = Duration::from_secs(30 * 24 * 3600);

/// The most bytes of one file [`files`] reads back (its end when longer).
pub const READ_MAX: u64 = 8 << 20;

/// Loop `loop_id`'s folder under `loops` (checked: [`valid_loop_id`]).
pub fn folder(loops: &Path, loop_id: &str) -> anyhow::Result<PathBuf> {
    if !valid_loop_id(loop_id) {
        anyhow::bail!("{loop_id:?} is no loop id");
    }
    Ok(loops.join(loop_id))
}

/// Makes `loops` and loop `loop_id`'s folder (private); returns the folder.
pub fn make(loops: &Path, loop_id: &str) -> anyhow::Result<PathBuf> {
    let dir = folder(loops, loop_id)?;
    twr_core::paths::ensure_private_dir(loops)?;
    twr_core::paths::ensure_private_dir(&dir)?;
    Ok(dir)
}

/// File `name` of loop `loop_id`'s folder (checked: [`valid_loop_file`]).
pub fn file(loops: &Path, loop_id: &str, name: &str) -> anyhow::Result<PathBuf> {
    if !valid_loop_file(name) {
        anyhow::bail!("{name:?} is no loop file name");
    }
    Ok(folder(loops, loop_id)?.join(name))
}

/// Writes `data` as file `name` of loop `loop_id`'s folder (made when missing), through a
/// temporary file and a rename.
pub fn write_file(loops: &Path, loop_id: &str, name: &str, data: &[u8]) -> anyhow::Result<()> {
    let path = file(loops, loop_id, name)?;
    let dir = make(loops, loop_id)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::io::Write::write_all(&mut tmp, data)?;
    tmp.persist(&path)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {}", path.display(), e.error))?;
    Ok(())
}

/// The digest of `path`'s content, if it is a file that can be read.
fn digest_of(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|b| twr_core::blob::blob_ref(&b))
}

/// The end of `path`, at most `max` bytes, and its whole size.
fn read_end(path: &Path, max: u64) -> std::io::Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a file"));
    }
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok((buf, len))
}

/// `Op::LoopFiles` on `loops`: writes each of `put` that has data, says which of the others
/// the folder lacks with that digest (`missing`), then reads back `read` (each at most `max`
/// bytes, at most [`READ_MAX`], its end when longer; a file that is not there is left out):
/// `{dir, missing, read: [{name, data, bytes, truncated}]}`.
pub fn files(
    loops: &Path,
    loop_id: &str,
    put: &[LoopFile],
    read: &[String],
    max: u64,
) -> Result<Value, CmdError> {
    let failed = |e: anyhow::Error| CmdError::new(cmd_errors::FAILED, format!("{e:#}"));
    let dir = folder(loops, loop_id).map_err(failed)?;
    let mut sent = 0usize;
    for f in put {
        if !valid_loop_file(&f.name) {
            return Err(failed(anyhow::anyhow!("{:?} is no loop file name", f.name)));
        }
        if let Some(data) = &f.data {
            sent += data.len() / 4 * 3;
            if sent > LOOP_FILES_MAX + LOOP_FILES_MAX / 2 {
                return Err(failed(anyhow::anyhow!(
                    "more than {LOOP_FILES_MAX} bytes of files in one request"
                )));
            }
        }
    }
    for n in read {
        if !valid_loop_file(n) {
            return Err(failed(anyhow::anyhow!("{n:?} is no loop file name")));
        }
    }
    let mut missing = Vec::new();
    for f in put {
        match &f.data {
            Some(data) => {
                let bytes = twr_core::b64::decode(data)
                    .map_err(|e| failed(anyhow::anyhow!("{}'s data is no base64: {e}", f.name)))?;
                if twr_core::blob::blob_ref(&bytes) != f.digest {
                    return Err(failed(anyhow::anyhow!(
                        "{}'s data does not match its digest",
                        f.name
                    )));
                }
                write_file(loops, loop_id, &f.name, &bytes).map_err(failed)?;
            }
            None => {
                if digest_of(&dir.join(&f.name)).as_deref() != Some(f.digest.as_str()) {
                    missing.push(f.name.clone());
                }
            }
        }
    }
    let max = if max == 0 {
        READ_MAX
    } else {
        max.min(READ_MAX)
    };
    let mut back = Vec::new();
    for n in read {
        if let Ok((data, len)) = read_end(&dir.join(n), max) {
            back.push(json!({
                "name": n,
                "data": twr_core::b64::encode(&data),
                "bytes": len,
                "truncated": len > data.len() as u64,
            }));
        }
    }
    // A folder used now is kept by `prune`.
    let _ = touch(&dir);
    Ok(json!({ "dir": dir, "missing": missing, "read": back }))
}

/// Marks `dir` as used now (its modification time), for [`prune`].
fn touch(dir: &Path) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let f = std::fs::File::open(dir)?;
    f.set_modified(std::time::SystemTime::now())
}

/// Removes loop `loop_id`'s folder (`Op::LoopForget`); a folder that is not there is fine.
pub fn forget(loops: &Path, loop_id: &str) -> anyhow::Result<()> {
    let dir = folder(loops, loop_id)?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("cannot remove {}: {e}", dir.display())),
    }
}

/// Removes the loop folders under `loops` that nobody used for [`KEEP_UNUSED`] (their thread
/// was deleted while the box was away, say). Best effort.
pub fn prune(loops: &Path) {
    let Ok(entries) = std::fs::read_dir(loops) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !valid_loop_id(&name) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > KEEP_UNUSED);
        if old && e.file_type().is_ok_and(|t| t.is_dir()) {
            if let Err(err) = std::fs::remove_dir_all(e.path()) {
                tracing::warn!(folder = %e.path().display(), error = %err, "cannot remove an old loop folder");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(name: &str, data: Option<&[u8]>, digest_of: &[u8]) -> LoopFile {
        LoopFile {
            name: name.into(),
            digest: twr_core::blob::blob_ref(digest_of),
            data: data.map(twr_core::b64::encode),
        }
    }

    #[test]
    fn files_are_written_checked_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let loops = dir.path().join("loops");
        let r = files(
            &loops,
            "L1",
            &[
                put("ledger.md", Some(b"# Ledger\n"), b"# Ledger\n"),
                put("baseline-api.patch", None, b"patch"),
            ],
            &["ledger.md".into(), "nope.log".into()],
            0,
        )
        .unwrap();
        assert_eq!(r["missing"], json!(["baseline-api.patch"]));
        assert_eq!(r["read"][0]["name"], "ledger.md");
        assert_eq!(
            twr_core::b64::decode(r["read"][0]["data"].as_str().unwrap()).unwrap(),
            b"# Ledger\n"
        );
        assert_eq!(r["read"].as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::read(loops.join("L1").join("ledger.md")).unwrap(),
            b"# Ledger\n"
        );
        // Checked by digest: there now.
        files(
            &loops,
            "L1",
            &[put("baseline-api.patch", Some(b"patch"), b"patch")],
            &[],
            0,
        )
        .unwrap();
        let r = files(
            &loops,
            "L1",
            &[put("baseline-api.patch", None, b"patch")],
            &[],
            0,
        )
        .unwrap();
        assert_eq!(r["missing"], json!([]));
        // A read is its end, cut at `max`.
        let r = files(&loops, "L1", &[], &["ledger.md".into()], 3).unwrap();
        assert_eq!(r["read"][0]["truncated"], true);
        assert_eq!(r["read"][0]["bytes"], 9);
        assert_eq!(
            twr_core::b64::decode(r["read"][0]["data"].as_str().unwrap()).unwrap(),
            b"er\n"
        );
        forget(&loops, "L1").unwrap();
        assert!(!loops.join("L1").exists());
        forget(&loops, "L1").unwrap();
    }

    #[test]
    fn names_never_leave_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let loops = dir.path().join("loops");
        for bad in ["../x", "a/b", ".hidden", "", "x\0y", &"y".repeat(200)] {
            assert!(
                files(&loops, "L1", &[put(bad, Some(b"x"), b"x")], &[], 0).is_err(),
                "{bad:?}"
            );
            assert!(
                files(&loops, "L1", &[], &[bad.to_string()], 0).is_err(),
                "{bad:?}"
            );
        }
        for bad in ["..", "a/b", "", "x y"] {
            assert!(files(&loops, bad, &[], &[], 0).is_err(), "{bad:?}");
            assert!(forget(&loops, bad).is_err(), "{bad:?}");
        }
        // Data that does not match its digest, or is no base64, is refused.
        assert!(files(&loops, "L1", &[put("a.md", Some(b"x"), b"y")], &[], 0).is_err());
        let bad = LoopFile {
            name: "a.md".into(),
            digest: "sha256:00".into(),
            data: Some("%%%".into()),
        };
        assert!(files(&loops, "L1", &[bad], &[], 0).is_err());
        // A symbolic link in the folder is never read through.
        let l1 = make(&loops, "L1").unwrap();
        std::fs::write(dir.path().join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), l1.join("link.log")).unwrap();
        let r = files(&loops, "L1", &[], &["link.log".into()], 0).unwrap();
        assert_eq!(r["read"], json!([]));
    }

    #[test]
    fn too_much_data_in_one_request_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; LOOP_FILES_MAX];
        let files_in = vec![
            put("a.patch", Some(&big), &big),
            put("b.patch", Some(&big), &big),
        ];
        assert!(files(dir.path(), "L1", &files_in, &[], 0).is_err());
        assert!(!dir.path().join("L1").join("a.patch").exists());
    }
}

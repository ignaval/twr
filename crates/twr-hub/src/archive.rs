//! The archive of the hub's log: `archive.db` beside `twr.db`, where the agent events the hub
//! no longer reads go (`hub::prune`), with the blobs only they reach.
//!
//! Tables (schema version [`ARCHIVE_SCHEMA_VERSION`], its own, in `PRAGMA user_version`):
//!
//! - `events`: the columns of `twr.db`'s `events` ([`crate::store`]), primary key
//!   `(thread, seq)`. A row is written once (`INSERT OR IGNORE`: copying a row again is
//!   harmless) and only ever read after that.
//! - `blobs(hash, bytes)`: as in `twr.db`.
//!
//! An event is in `twr.db`, or here, or in both for a while (a compaction copies before it
//! deletes, and a crash in between leaves both copies, the same row): readers take the one in
//! `twr.db` when both have it. No triggers: the archive holds no control event and nothing
//! folds it. It is private to its owner as `twr.db` is (0600).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use rusqlite::Connection;

/// The archive's schema version (`PRAGMA user_version`).
pub const ARCHIVE_SCHEMA_VERSION: i32 = 1;

const ARCHIVE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    thread TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    id     TEXT NOT NULL UNIQUE,
    v      INTEGER NOT NULL,
    ts     TEXT NOT NULL,
    turn   INTEGER,
    source TEXT NOT NULL,
    kind   TEXT NOT NULL,
    data   TEXT NOT NULL,
    raw    TEXT,
    PRIMARY KEY (thread, seq)
);
CREATE TABLE IF NOT EXISTS blobs (
    hash  TEXT PRIMARY KEY,
    bytes BLOB NOT NULL
);
"#;

/// The archive of the log at `db`: `archive.db` in the same directory.
pub fn path_of(db: &Path) -> PathBuf {
    db.with_file_name("archive.db")
}

/// Opens (creating if needed, mode 0600) the archive at `path` and checks its schema.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    crate::store::create_private(path)?;
    let conn = crate::store::connect(path)?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > ARCHIVE_SCHEMA_VERSION {
        bail!(
            "archive {} has schema version {version}, newer than this twr's ({ARCHIVE_SCHEMA_VERSION})",
            path.display()
        );
    }
    if version < ARCHIVE_SCHEMA_VERSION {
        conn.execute_batch(&format!(
            "BEGIN; {ARCHIVE_SCHEMA} PRAGMA user_version = {ARCHIVE_SCHEMA_VERSION}; COMMIT;"
        ))
        .with_context(|| format!("creating the archive {}", path.display()))?;
    }
    crate::store::restrict_db_files(path)?;
    Ok(conn)
}

/// A reader of the archive at `path`: `None` while there is none (nothing was archived yet),
/// or when it is of another schema (logged).
pub fn open_reader(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    let conn = match crate::store::connect(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "cannot open the archive");
            return None;
        }
    };
    let version: rusqlite::Result<i32> =
        conn.pragma_query_value(None, "user_version", |r| r.get(0));
    match version {
        Ok(ARCHIVE_SCHEMA_VERSION) => Some(conn),
        Ok(0) => None,
        Ok(v) => {
            tracing::warn!(
                version = v,
                "the archive is of another schema; not reading it"
            );
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the archive");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The archive is made private and with its schema once; a reader sees none before it.
    #[test]
    fn the_archive_is_private_and_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_of(&dir.path().join("twr.db"));
        assert_eq!(path, dir.path().join("archive.db"));
        assert!(open_reader(&path).is_none());
        let conn = open(&path).unwrap();
        conn.execute("INSERT INTO blobs (hash, bytes) VALUES ('h', x'00')", [])
            .unwrap();
        drop(conn);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let again = open(&path).unwrap();
        let n: i64 = again
            .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(open_reader(&path).is_some());
        again
            .pragma_update(None, "user_version", ARCHIVE_SCHEMA_VERSION + 1)
            .unwrap();
        assert!(open(&path).is_err());
        assert!(open_reader(&path).is_none());
    }
}

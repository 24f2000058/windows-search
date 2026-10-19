//! SQLite persistence for index entries and per-volume checkpoints.
//!
//! * Schema version lives in `PRAGMA user_version`; [`Store::open`] migrates
//!   forward and refuses databases written by a newer version.
//! * A database that fails to open or fails `quick_check` is moved aside
//!   (never deleted) and a fresh one is created; the caller is told, and the
//!   affected volumes rescan.
//! * WAL mode with `synchronous=NORMAL`: a power loss may drop the last
//!   committed batches, which is safe because the checkpoint is committed in
//!   the same transaction as the entries it covers and journal application is
//!   idempotent — we simply replay from the older checkpoint.

use crate::model::{FileRecord, VolumeState};
use crate::volume::StoreOp;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    NewerSchema(i64),
    /// Fault injection: simulated crash before commit.
    InjectedFault,
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::NewerSchema(v) => write!(f, "index database schema {v} is newer than supported {SCHEMA_VERSION}"),
            StoreError::InjectedFault => write!(f, "injected fault"),
        }
    }
}

impl std::error::Error for StoreError {}

pub type Result<T> = std::result::Result<T, StoreError>;

/// What happened while opening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened,
    Created,
    Migrated { from: i64 },
    /// The previous file was unreadable/corrupt and was moved to this path.
    Recovered { moved_to: PathBuf },
}

pub struct Store {
    conn: Connection,
    /// Fault injection: fail a commit after this many more ops.
    pub fail_after_ops: Option<usize>,
}

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS volumes (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    root_name TEXT NOT NULL,
    state TEXT NOT NULL,
    a INTEGER NOT NULL DEFAULT 0,
    b INTEGER NOT NULL DEFAULT 0,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS entries (
    volume_id INTEGER NOT NULL,
    frn INTEGER NOT NULL,
    parent INTEGER NOT NULL,
    -- Exact collation: NTFS directories can be case-sensitive (WSL).
    name TEXT NOT NULL,
    attrs INTEGER NOT NULL,
    size INTEGER,
    mtime INTEGER,
    PRIMARY KEY (volume_id, frn, parent, name)
) WITHOUT ROWID;
";

impl Store {
    pub fn open(path: &Path) -> Result<(Store, OpenOutcome)> {
        let existed = path.exists();
        match Self::try_open(path) {
            Ok((s, outcome)) => Ok((s, if existed { outcome } else { OpenOutcome::Created })),
            Err(StoreError::NewerSchema(v)) => Err(StoreError::NewerSchema(v)),
            Err(_) if existed => {
                let moved_to = path.with_extension(format!("corrupt-{}", std::process::id()));
                let _ = std::fs::rename(path, &moved_to);
                for ext in ["db-wal", "db-shm"] {
                    let _ = std::fs::remove_file(path.with_extension(ext));
                }
                let (s, _) = Self::try_open(path)?;
                Ok((s, OpenOutcome::Recovered { moved_to }))
            }
            Err(e) => Err(e),
        }
    }

    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        let mut s = Store { conn, fail_after_ops: None };
        s.migrate()?;
        Ok(s)
    }

    fn try_open(path: &Path) -> Result<(Store, OpenOutcome)> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let ok: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if ok != "ok" {
            return Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        let mut s = Store { conn, fail_after_ops: None };
        let outcome = s.migrate()?;
        Ok((s, outcome))
    }

    fn migrate(&mut self) -> Result<OpenOutcome> {
        let v: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema(v));
        }
        if v == SCHEMA_VERSION {
            return Ok(OpenOutcome::Opened);
        }
        let tx = self.conn.transaction()?;
        if v < 1 {
            tx.execute_batch(SCHEMA_V1)?;
        }
        // Future migrations: `if v < 2 { ... }`, each idempotent.
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(if v == 0 { OpenOutcome::Created } else { OpenOutcome::Migrated { from: v } })
    }

    /// Returns (volume db id, stored state), creating the row if needed.
    pub fn volume(&mut self, key: &str, root_name: &str) -> Result<(i64, VolumeState)> {
        let row: Option<(i64, String, i64, i64, String)> = self
            .conn
            .query_row("SELECT id, state, a, b, detail FROM volumes WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        if let Some((id, k, a, b, d)) = row {
            return Ok((id, VolumeState::decode(&k, a, b, &d)));
        }
        let (k, a, b, d) = VolumeState::NotIndexed.encode();
        self.conn.execute("INSERT INTO volumes (key, root_name, state, a, b, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![key, root_name, k, a, b, d])?;
        Ok((self.conn.last_insert_rowid(), VolumeState::NotIndexed))
    }

    pub fn state(&self, vol: i64) -> Result<VolumeState> {
        let (k, a, b, d): (String, i64, i64, String) = self.conn.query_row("SELECT state, a, b, detail FROM volumes WHERE id = ?1", [vol], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(VolumeState::decode(&k, a, b, &d))
    }

    /// Applies `ops` and (optionally) the new state in one transaction.
    pub fn commit(&mut self, vol: i64, clear_first: bool, ops: &[StoreOp], state: Option<&VolumeState>) -> Result<()> {
        let tx = self.conn.transaction()?;
        if clear_first {
            tx.execute("DELETE FROM entries WHERE volume_id = ?1", [vol])?;
        }
        {
            let mut put = tx.prepare_cached("INSERT OR REPLACE INTO entries (volume_id, frn, parent, name, attrs, size, mtime) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            let mut del_link = tx.prepare_cached("DELETE FROM entries WHERE volume_id = ?1 AND frn = ?2 AND parent = ?3 AND name = ?4")?;
            let mut del_file = tx.prepare_cached("DELETE FROM entries WHERE volume_id = ?1 AND frn = ?2")?;
            let mut set_meta = tx.prepare_cached("UPDATE entries SET attrs = ?3, size = ?4, mtime = ?5 WHERE volume_id = ?1 AND frn = ?2")?;
            for op in ops {
                if let Some(n) = self.fail_after_ops.as_mut() {
                    if *n == 0 {
                        // Dropping `tx` without commit rolls back, exactly as
                        // a crash before COMMIT would.
                        return Err(StoreError::InjectedFault);
                    }
                    *n -= 1;
                }
                match op {
                    StoreOp::PutLink { frn, parent, name, attrs, size, mtime } => {
                        put.execute(params![vol, *frn as i64, *parent as i64, name, attrs, size.map(|s| s as i64), mtime])?;
                    }
                    StoreOp::DelLink { frn, parent, name } => {
                        del_link.execute(params![vol, *frn as i64, *parent as i64, name])?;
                    }
                    StoreOp::DelFile { frn } => {
                        del_file.execute(params![vol, *frn as i64])?;
                    }
                    StoreOp::SetMeta { frn, attrs, size, mtime } => {
                        set_meta.execute(params![vol, *frn as i64, attrs, size.map(|s| s as i64), mtime])?;
                    }
                }
            }
        }
        if let Some(st) = state {
            let (k, a, b, d) = st.encode();
            tx.execute("UPDATE volumes SET state = ?2, a = ?3, b = ?4, detail = ?5 WHERE id = ?1", params![vol, k, a, b, d])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Streams stored links of a volume.
    pub fn load(&self, vol: i64, mut sink: impl FnMut(FileRecord)) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT frn, parent, name, attrs, size, mtime FROM entries WHERE volume_id = ?1")?;
        let mut rows = stmt.query([vol])?;
        let mut n = 0;
        while let Some(r) = rows.next()? {
            let size: Option<i64> = r.get(4)?;
            sink(FileRecord { frn: r.get::<_, i64>(0)? as u64, parent: r.get::<_, i64>(1)? as u64, name: r.get(2)?, attrs: r.get(3)?, size: size.map(|s| s as u64), mtime: r.get(5)? });
            n += 1;
        }
        Ok(n)
    }

    pub fn entry_count(&self, vol: i64) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM entries WHERE volume_id = ?1", [vol], |r| r.get(0))?)
    }

    /// Removes a volume and its entries (e.g. user excluded it).
    pub fn forget_volume(&mut self, vol: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM entries WHERE volume_id = ?1", [vol])?;
        tx.execute("DELETE FROM volumes WHERE id = ?1", [vol])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-store-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d.join("index.db")
    }

    #[test]
    fn creates_reopens_and_refuses_newer_schema() {
        let p = tmp("schema");
        let _ = std::fs::remove_file(&p);
        let (_, o) = Store::open(&p).unwrap();
        assert_eq!(o, OpenOutcome::Created);
        let (s, o) = Store::open(&p).unwrap();
        assert_eq!(o, OpenOutcome::Opened);
        s.conn.pragma_update(None, "user_version", 99).unwrap();
        drop(s);
        assert!(matches!(Store::open(&p), Err(StoreError::NewerSchema(99))));
        assert!(p.exists(), "a newer database must not be moved or destroyed");
    }

    #[test]
    fn corrupt_file_is_moved_aside_not_deleted() {
        let p = tmp("corrupt");
        std::fs::write(&p, b"this is definitely not a sqlite database, just garbage bytes..........").unwrap();
        let (s, o) = Store::open(&p).unwrap();
        let OpenOutcome::Recovered { moved_to } = o else { panic!("{o:?}") };
        assert!(moved_to.exists());
        assert_eq!(s.state(1).ok(), None);
    }

    #[test]
    fn injected_fault_rolls_back_ops_and_checkpoint() {
        let mut s = Store::open_in_memory().unwrap();
        let (v, _) = s.volume("vol", "C:").unwrap();
        let put = |n: &str| StoreOp::PutLink { frn: 1, parent: 5, name: n.into(), attrs: 0, size: Some(1), mtime: Some(2) };
        s.commit(v, false, &[put("a")], Some(&VolumeState::Live { journal_id: 1, next_usn: 10 })).unwrap();
        s.fail_after_ops = Some(1);
        assert!(s.commit(v, false, &[put("b"), put("c")], Some(&VolumeState::Live { journal_id: 1, next_usn: 20 })).is_err());
        s.fail_after_ops = None;
        assert_eq!(s.entry_count(v).unwrap(), 1);
        assert_eq!(s.state(v).unwrap(), VolumeState::Live { journal_id: 1, next_usn: 10 });
    }
}

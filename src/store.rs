//! Session metadata and user decisions in a private SQLite catalog.
//!
//! Pi JSONL files stay authoritative for transcript text; this store keeps
//! names, previews, resume states, notes, undo rows, source cutoffs, and
//! UI preferences. History never becomes "new" on reimport, and explicit
//! Done decisions survive reimports and restarts.
use crate::{
    model::{ResumeState, Session},
    paths,
    pi::Cursor,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Store {
    conn: Connection,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct Source {
    pub path: PathBuf,
    pub cutoff: i64,
    pub complete: bool,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        paths::secure_dir(root)?;
        // SQLite NOFOLLOW also rejects ancestor aliases such as macOS /var.
        // Canonicalize the validated directory, never the database file itself.
        let path = paths::canonical(root).join("sessions.db");
        use std::os::unix::fs::OpenOptionsExt;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => paths::private_file(&path)?,
            Err(e) => return Err(e.into()),
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 2,
            "sessions.db schema is newer than this program"
        );
        if version < 2 {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            // Another client may have initialized the file while we waited.
            let locked_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                locked_version <= 2,
                "sessions.db schema is newer than this program"
            );
            if locked_version == 0 {
                conn.execute_batch("CREATE TABLE sources(path TEXT PRIMARY KEY,cutoff INTEGER NOT NULL,complete INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE sessions(id TEXT PRIMARY KEY,file TEXT UNIQUE NOT NULL,source TEXT NOT NULL REFERENCES sources(path),native_id TEXT NOT NULL,cwd TEXT NOT NULL,updated INTEGER NOT NULL,cursor TEXT NOT NULL,state TEXT NOT NULL,note TEXT NOT NULL DEFAULT '',done_revision TEXT NOT NULL DEFAULT '0',available INTEGER NOT NULL DEFAULT 1);
          CREATE INDEX sessions_project ON sessions(cwd,updated DESC,id);
          CREATE INDEX sessions_native ON sessions(native_id);
          CREATE INDEX sessions_state ON sessions(state,updated DESC,id);
          CREATE TABLE undo(session TEXT PRIMARY KEY REFERENCES sessions(id),state TEXT NOT NULL,note TEXT NOT NULL,revision TEXT NOT NULL);
          CREATE TABLE preferences(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          PRAGMA user_version=1;")?;
            }
            if locked_version <= 1 {
                conn.execute_batch(
                    "ALTER TABLE sessions ADD COLUMN done_at INTEGER NOT NULL DEFAULT 0;
                    ALTER TABLE undo ADD COLUMN done_at INTEGER NOT NULL DEFAULT 0;
                    PRAGMA user_version=2;",
                )?;
            }
            conn.execute_batch("COMMIT")?;
        }
        Ok(Self { conn })
    }
    pub fn add_source(&self, path: &Path) -> Result<()> {
        ensure!(path.is_absolute(), "source must be an absolute path");
        let path = paths::canonical(path);
        self.conn.execute(
            "INSERT OR IGNORE INTO sources(path,cutoff) VALUES(?,?)",
            params![path.to_string_lossy(), paths::now()],
        )?;
        Ok(())
    }
    pub fn sources(&self) -> Result<Vec<Source>> {
        Ok(self
            .conn
            .prepare("SELECT path,cutoff,complete FROM sources ORDER BY path")?
            .query_map([], |r| {
                Ok(Source {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    cutoff: r.get(1)?,
                    complete: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn complete(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET complete=1 WHERE path=?",
            [path.to_string_lossy()],
        )?;
        Ok(())
    }
    pub fn cursor(&self, file: &Path) -> Result<Option<Cursor>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT cursor FROM sessions WHERE file=?",
                [file.to_string_lossy()],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn ingest(&self, source: &Source, cursor: &Cursor) -> Result<()> {
        let meta = &cursor.metadata;
        let file = meta.file.to_string_lossy();
        // Replacement with a different native ID must not inherit a completed
        // conversation's decisions. Retain the old row as missing history.
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id,native_id FROM sessions WHERE file=?",
                [file.as_ref()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            if let Some((id, native)) = &existing
                && native != &meta.native_id
            {
                self.conn.execute(
                    "UPDATE sessions SET file=file||'#replaced:'||id,available=0 WHERE id=?",
                    [id],
                )?;
            }
            let id = existing
                .filter(|(_, native)| *native == meta.native_id)
                .map(|(id, _)| id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            // Discovery is not a work decision: only an explicit user action
            // may mark a conversation To resume, regardless of its creation time.
            let state = ResumeState::History;
            self.conn.execute("INSERT INTO sessions(id,file,source,native_id,cwd,updated,cursor,state) VALUES(?,?,?,?,?,?,?,?)
                ON CONFLICT(file) DO UPDATE SET native_id=excluded.native_id,cwd=excluded.cwd,updated=excluded.updated,cursor=excluded.cursor,available=1",
                params![id,file,source.path.to_string_lossy(),meta.native_id,meta.cwd,meta.updated,serde_json::to_string(cursor)?,serde_json::to_string(&state)?])?;
            Ok(())
        })();
        if result.is_ok() {
            self.conn.execute_batch("COMMIT")?;
        } else {
            self.conn.execute_batch("ROLLBACK")?;
        }
        result
    }
    pub fn mark_missing(&self, file: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET available=0 WHERE file=?",
            [file.to_string_lossy()],
        )?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<Session>> {
        let mut stmt=self.conn.prepare("SELECT id,cursor,state,note,done_revision,done_at,available FROM sessions ORDER BY updated DESC,id")?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(id, cursor, state, note, rev, done_at, available)| {
                Ok(Session {
                    id,
                    metadata: serde_json::from_str::<Cursor>(&cursor)?.metadata,
                    state: serde_json::from_str(&state)?,
                    note,
                    done_revision: rev.parse()?,
                    done_at,
                    available,
                    presence_verified: false,
                    bindings: vec![],
                    probable: vec![],
                })
            })
            .collect()
    }
    pub fn resolve(&self, target: &str) -> Result<Session> {
        let matches = self
            .list()?
            .into_iter()
            .filter(|s| s.id == target || s.metadata.native_id == target)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() <= 1,
            "ambiguous Pi id: use the catalog id to pick the exact file"
        );
        matches.into_iter().next().context("session not found")
    }
    pub fn change(
        &self,
        target: &str,
        state: Option<ResumeState>,
        note: Option<&str>,
    ) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let session = self.resolve(target)?;
            let state = state.unwrap_or(session.state);
            let note = note.unwrap_or(&session.note);
            ensure!(
                note.chars().count() <= 4000,
                "Note is too long (4000 characters maximum)"
            );
            let (revision, done_at) = if state == ResumeState::Done {
                if session.state == ResumeState::Done {
                    (session.done_revision, session.done_at)
                } else {
                    (session.metadata.revision, paths::now())
                }
            } else {
                (0, 0)
            };
            if state == session.state && note == session.note {
                return Ok(());
            }
            self.conn.execute("INSERT INTO undo(session,state,note,revision,done_at) SELECT id,state,note,done_revision,done_at FROM sessions WHERE id=? ON CONFLICT(session) DO UPDATE SET state=excluded.state,note=excluded.note,revision=excluded.revision,done_at=excluded.done_at",[&session.id])?;
            self.conn.execute(
                "UPDATE sessions SET state=?,note=?,done_revision=?,done_at=? WHERE id=?",
                params![
                    serde_json::to_string(&state)?,
                    note,
                    revision.to_string(),
                    done_at,
                    session.id
                ],
            )?;
            Ok(())
        })();
        if result.is_ok() {
            self.conn.execute_batch("COMMIT")?;
        } else {
            self.conn.execute_batch("ROLLBACK")?;
        }
        result
    }
    pub fn undo(&self, target: &str) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let id = self.resolve(target)?.id;
            let prev: Option<(String, String, String, i64)> = self
                .conn
                .query_row(
                    "SELECT state,note,revision,done_at FROM undo WHERE session=?",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let (state, note, revision, done_at) = prev.context("nothing to undo")?;
            self.conn.execute(
                "UPDATE sessions SET state=?,note=?,done_revision=?,done_at=? WHERE id=?",
                params![state, note, revision, done_at, id],
            )?;
            self.conn
                .execute("DELETE FROM undo WHERE session=?", [id])?;
            Ok(())
        })();
        if result.is_ok() {
            self.conn.execute_batch("COMMIT")?;
        } else {
            self.conn.execute_batch("ROLLBACK")?;
        }
        result
    }
    pub fn preference(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM preferences WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn set_preference(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO preferences(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_and_new_sessions_stay_history_until_manually_marked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let store = Store::open(&root).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        let source = store.sources().unwrap().remove(0);
        let mut cursor = Cursor::default();
        cursor.metadata.file = "/fixtures/old.jsonl".into();
        cursor.metadata.native_id = "old".into();
        cursor.metadata.created = source.cutoff - 100;
        store.ingest(&source, &cursor).unwrap();
        cursor.metadata.file = "/fixtures/new.jsonl".into();
        cursor.metadata.native_id = "new".into();
        cursor.metadata.created = source.cutoff + 100;
        store.ingest(&source, &cursor).unwrap();
        assert_eq!(store.resolve("old").unwrap().state, ResumeState::History);
        assert_eq!(store.resolve("new").unwrap().state, ResumeState::History);
        store
            .change("new", None, Some("A note is not a resume decision"))
            .unwrap();
        store.ingest(&source, &cursor).unwrap();
        assert_eq!(store.resolve("new").unwrap().state, ResumeState::History);
        store
            .change("new", Some(ResumeState::Resume), None)
            .unwrap();
        store.ingest(&source, &cursor).unwrap();
        assert_eq!(store.resolve("new").unwrap().state, ResumeState::Resume);
        let cutoff = source.cutoff;
        drop(store);
        let store = Store::open(&root).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        assert_eq!(store.sources().unwrap()[0].cutoff, cutoff);
        store
            .mark_missing(Path::new("/fixtures/new.jsonl"))
            .unwrap();
        let session = store.resolve("new").unwrap();
        assert!(!session.available);
        assert_eq!(session.state, ResumeState::Resume);
    }
    #[test]
    fn conflicts_and_failed_mutations_do_not_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        let source = store.sources().unwrap().remove(0);
        let mut cursor = Cursor::default();
        cursor.metadata.native_id = "same-native".into();
        cursor.metadata.file = "/fixtures/one.jsonl".into();
        store.ingest(&source, &cursor).unwrap();
        cursor.metadata.file = "/fixtures/two.jsonl".into();
        store.ingest(&source, &cursor).unwrap();
        assert!(store.resolve("same-native").is_err());
        let id = store.list().unwrap()[0].id.clone();
        assert!(store.change("missing", None, Some("note")).is_err());
        store.change(&id, None, Some("still works")).unwrap();
        assert_eq!(store.resolve(&id).unwrap().note, "still works");
    }
    #[test]
    fn existing_databases_are_untouched_and_symlinks_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        paths::secure_dir(&root).unwrap();
        fs::write(root.join("board.db"), b"preserved board").unwrap();
        fs::write(root.join("library.db"), b"preserved library").unwrap();
        let store = Store::open(&root).unwrap();
        drop(store);
        assert_eq!(fs::read(root.join("board.db")).unwrap(), b"preserved board");
        assert_eq!(
            fs::read(root.join("library.db")).unwrap(),
            b"preserved library"
        );
        fs::remove_file(root.join("sessions.db")).unwrap();
        std::os::unix::fs::symlink(root.join("board.db"), root.join("sessions.db")).unwrap();
        assert!(Store::open(&root).is_err());
        assert_eq!(fs::read(root.join("board.db")).unwrap(), b"preserved board");
    }
    #[test]
    fn catching_up_old_history_is_not_a_new_exchange_after_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        let source = store.sources().unwrap().remove(0);
        let mut cursor = Cursor::default();
        cursor.metadata.file = "/fixtures/partial.jsonl".into();
        cursor.metadata.native_id = "partial".into();
        cursor.metadata.updated = paths::now() - 1000;
        store.ingest(&source, &cursor).unwrap();
        store
            .change("partial", Some(ResumeState::Done), None)
            .unwrap();
        let done_at = store.resolve("partial").unwrap().done_at;
        cursor.metadata.revision = 1;
        store.ingest(&source, &cursor).unwrap();
        assert!(!store.resolve("partial").unwrap().changed_after_done());
        cursor.metadata.revision = 2;
        cursor.metadata.updated = done_at + 100;
        store.ingest(&source, &cursor).unwrap();
        assert!(store.resolve("partial").unwrap().changed_after_done());
        store.change("partial", None, Some("next step")).unwrap();
        let session = store.resolve("partial").unwrap();
        assert!(session.changed_after_done());
        assert_eq!(session.done_at, done_at);
        store.undo("partial").unwrap();
        assert!(store.resolve("partial").unwrap().changed_after_done());
    }
    #[test]
    fn schema_one_upgrade_retains_decisions_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let store = Store::open(&root).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        let source = store.sources().unwrap().remove(0);
        let mut cursor = Cursor::default();
        cursor.metadata.file = "/fixtures/one.jsonl".into();
        cursor.metadata.native_id = "one".into();
        store.ingest(&source, &cursor).unwrap();
        store
            .change("one", Some(ResumeState::Done), Some("preserved"))
            .unwrap();
        store.conn.execute_batch("ALTER TABLE sessions DROP COLUMN done_at; ALTER TABLE undo DROP COLUMN done_at; PRAGMA user_version=1;").unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.resolve("one").unwrap().state, ResumeState::Done);
        assert_eq!(store.resolve("one").unwrap().note, "preserved");
        store.undo("one").unwrap();
        assert_eq!(store.resolve("one").unwrap().state, ResumeState::History);
    }
    #[test]
    fn decisions_survive_reimport_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let store = Store::open(&root).unwrap();
        store.add_source(Path::new("/fixtures")).unwrap();
        let source = store.sources().unwrap().remove(0);
        let mut cursor = Cursor::default();
        cursor.metadata.native_id = "pi-native".into();
        cursor.metadata.file = "/fixtures/session.jsonl".into();
        cursor.metadata.created = source.cutoff - 1;
        store.ingest(&source, &cursor).unwrap();
        assert_eq!(store.list().unwrap()[0].state, ResumeState::History);
        store
            .change("pi-native", Some(ResumeState::Resume), None)
            .unwrap();
        store
            .change("pi-native", Some(ResumeState::Done), Some("next"))
            .unwrap();
        cursor.metadata.revision = 2;
        cursor.metadata.updated = paths::now() + 100;
        store.ingest(&source, &cursor).unwrap();
        let session = store.resolve("pi-native").unwrap();
        assert_eq!(session.state, ResumeState::Done);
        assert!(session.changed_after_done());
        store.undo("pi-native").unwrap();
        assert_eq!(
            store.resolve("pi-native").unwrap().state,
            ResumeState::Resume
        );
        assert_eq!(store.resolve("pi-native").unwrap().note, "");
        drop(store);
        let reopened = Store::open(&root).unwrap();
        assert_eq!(
            reopened.resolve("pi-native").unwrap().state,
            ResumeState::Resume
        );
    }
}

//! SQLite: schema, migrations and the generated columns that mirror the
//! field registry.

use crate::registry::FIELDS;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::Mutex;

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA_V1: &str = r#"
CREATE TABLE user (
  id           INTEGER PRIMARY KEY,
  name         TEXT    NOT NULL UNIQUE,
  display_name TEXT    NOT NULL,
  created_at   INTEGER NOT NULL
);

CREATE TABLE entry (
  entry_id            INTEGER PRIMARY KEY,
  version             INTEGER NOT NULL DEFAULT 1,
  at                  INTEGER NOT NULL,
  duration_s          INTEGER CHECK (duration_s >= 0),
  text                TEXT    NOT NULL,
  fields              TEXT    NOT NULL DEFAULT '{}'
                      CHECK (json_valid(fields) AND json_type(fields) = 'object'),
  needs_attention_by  INTEGER NOT NULL DEFAULT 0 CHECK (needs_attention_by BETWEEN 0 AND 3),
  needs_attention_why TEXT,
  deleted             INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
  changed_at          INTEGER NOT NULL,
  changed_by          INTEGER NOT NULL REFERENCES user(id),
  CHECK ((needs_attention_by = 0) = (needs_attention_why IS NULL))
);
CREATE INDEX entry_at        ON entry(at) WHERE deleted = 0;
CREATE INDEX entry_attention ON entry(at) WHERE needs_attention_by != 0 AND deleted = 0;

CREATE TABLE entry_old (
  entry_id            INTEGER NOT NULL REFERENCES entry(entry_id),
  version             INTEGER NOT NULL,
  at                  INTEGER NOT NULL,
  duration_s          INTEGER,
  text                TEXT    NOT NULL,
  fields              TEXT    NOT NULL,
  needs_attention_by  INTEGER NOT NULL,
  needs_attention_why TEXT,
  deleted             INTEGER NOT NULL,
  changed_at          INTEGER NOT NULL,
  changed_by          INTEGER NOT NULL REFERENCES user(id),
  superseded_at       INTEGER NOT NULL,
  PRIMARY KEY (entry_id, version)
);

CREATE TRIGGER entry_keep_old BEFORE UPDATE ON entry
BEGIN
  SELECT RAISE(ABORT, 'version must increase by 1') WHERE NEW.version != OLD.version + 1;
  INSERT INTO entry_old (entry_id, version, at, duration_s, text, fields,
                         needs_attention_by, needs_attention_why, deleted,
                         changed_at, changed_by, superseded_at)
  VALUES (OLD.entry_id, OLD.version, OLD.at, OLD.duration_s, OLD.text, OLD.fields,
          OLD.needs_attention_by, OLD.needs_attention_why, OLD.deleted,
          OLD.changed_at, OLD.changed_by, NEW.changed_at);
END;

CREATE TRIGGER entry_no_delete BEFORE DELETE ON entry
BEGIN
  SELECT RAISE(ABORT, 'entries are not deleted; set deleted = 1');
END;

CREATE TABLE suggestion (
  id     INTEGER PRIMARY KEY,
  at     INTEGER NOT NULL,
  by     INTEGER NOT NULL REFERENCES user(id),
  text   TEXT    NOT NULL,
  status TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done', 'rejected')),
  note   TEXT
);

-- Authentication state.
CREATE TABLE passkey (
  id         INTEGER PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES user(id),
  name       TEXT    NOT NULL,
  cred_id    TEXT    NOT NULL UNIQUE,   -- base64url credential id
  credential TEXT    NOT NULL,          -- webauthn-rs Passkey, JSON
  created_at INTEGER NOT NULL,
  last_used  INTEGER
);
CREATE TABLE enroll_token (
  hash    TEXT    PRIMARY KEY,           -- sha256 of the token
  user_id INTEGER NOT NULL REFERENCES user(id),
  expires INTEGER NOT NULL
);
CREATE TABLE session (
  hash       TEXT    PRIMARY KEY,        -- sha256 of the cookie value
  user_id    INTEGER NOT NULL REFERENCES user(id),
  created_at INTEGER NOT NULL,
  expires    INTEGER NOT NULL
);
CREATE TABLE oauth_client (
  id            TEXT    PRIMARY KEY,
  secret_hash   TEXT,
  name          TEXT    NOT NULL,
  redirect_uris TEXT    NOT NULL,       -- JSON array
  auth_method   TEXT    NOT NULL,
  created_at    INTEGER NOT NULL,
  last_used     INTEGER
);
CREATE TABLE refresh_token (
  hash       TEXT    PRIMARY KEY,
  client_id  TEXT    NOT NULL,
  user_id    INTEGER NOT NULL REFERENCES user(id),
  passkey_id INTEGER,
  created_at INTEGER NOT NULL,
  expires    INTEGER NOT NULL
);
CREATE TABLE kv (
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
);
"#;

/// Web push: devices, per-user choices, what was sent, and the messages the
/// service worker fetches (pushes carry only a message id).
const SCHEMA_V2: &str = r#"
CREATE TABLE push_subscription (
  id         INTEGER PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES user(id),
  endpoint   TEXT    NOT NULL UNIQUE,
  p256dh     TEXT    NOT NULL,
  auth       TEXT    NOT NULL,
  label      TEXT    NOT NULL,
  created_at INTEGER NOT NULL,
  last_ok    INTEGER,
  last_error TEXT
);
CREATE TABLE push_pref (
  user_id INTEGER NOT NULL REFERENCES user(id),
  kind    TEXT    NOT NULL,
  enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  PRIMARY KEY (user_id, kind)
);
CREATE TABLE push_sent (
  user_id INTEGER NOT NULL REFERENCES user(id),
  kind    TEXT    NOT NULL,
  key     TEXT    NOT NULL,
  at      INTEGER NOT NULL,
  PRIMARY KEY (user_id, kind, key)
);
CREATE TABLE push_message (
  hash    TEXT    PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES user(id),
  title   TEXT    NOT NULL,
  body    TEXT    NOT NULL,
  url     TEXT    NOT NULL,
  expires INTEGER NOT NULL
);
"#;

pub const ENTRY_TABLES: [&str; 2] = ["entry", "entry_old"];

impl Db {
    pub fn open(path: &Path) -> Result<Db, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let db = Db { conn: Mutex::new(conn) };
        db.with(|c| {
            c.execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
            )?;
            migrate(c)
        })
        .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(db)
    }

    /// Runs `f` with the connection; calls are short, so a mutex suffices.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        let mut c = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut c)
    }

    /// A separate read-only connection, for the `sql` tool.
    pub fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
        let c = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 5000;")?;
        Ok(c)
    }
}

fn migrate(c: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        let tx = c.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        tx.execute_batch("PRAGMA user_version = 1")?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = c.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.execute_batch("PRAGMA user_version = 2")?;
        tx.commit()?;
    }
    sync_generated_columns(c)
}

/// Adds a virtual generated column for every registry field that does not
/// have one yet, to both entry tables.
pub fn sync_generated_columns(c: &Connection) -> rusqlite::Result<()> {
    for table in ENTRY_TABLES {
        let have = columns(c, table)?;
        for f in FIELDS {
            if !have.iter().any(|h| h == f.name) {
                c.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {name} {typ} GENERATED ALWAYS AS (fields ->> '$.{name}') VIRTUAL",
                    name = f.name,
                    typ = f.sql_type()
                ))?;
            }
        }
    }
    Ok(())
}

pub fn columns(c: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    // table_xinfo includes generated columns (table_info hides them).
    let mut st = c.prepare(&format!("PRAGMA table_xinfo({table})"))?;
    let rows = st.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_generated_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.sqlite")).unwrap();
        db.with(|c| {
            let e = columns(c, "entry")?;
            let o = columns(c, "entry_old")?;
            for f in FIELDS {
                assert!(e.contains(&f.name.to_string()), "entry lacks {}", f.name);
                assert!(o.contains(&f.name.to_string()), "entry_old lacks {}", f.name);
            }
            // entry_old = entry's columns + superseded_at, in any order
            let mut e2 = e.clone();
            e2.push("superseded_at".into());
            e2.sort();
            let mut o2 = o.clone();
            o2.sort();
            assert_eq!(e2, o2);
            Ok(())
        })
        .unwrap();
        // Reopening is idempotent.
        drop(db);
        Db::open(&dir.path().join("t.sqlite")).unwrap();
    }
}

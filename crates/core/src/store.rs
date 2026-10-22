//! The app's own data on disk: query history and saved queries, in one
//! SQLite file. Small and synchronous: every call is one short statement.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::DatabaseError;

/// History kept for each connection; older entries are removed.
const HISTORY_LIMIT: i64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HistoryEntry {
    pub id: i64,
    pub sql: String,
    /// Milliseconds since 1970.
    pub ran_at: i64,
    pub elapsed_ms: Option<u64>,
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedQuery {
    pub id: i64,
    pub name: String,
    pub sql: String,
    pub updated_at: i64,
}

#[derive(uniffi::Object)]
pub struct LocalStore {
    connection: Mutex<Connection>,
}

impl From<rusqlite::Error> for DatabaseError {
    fn from(error: rusqlite::Error) -> Self {
        DatabaseError::new(format!("The local store failed: {error}."))
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}

#[uniffi::export]
impl LocalStore {
    /// Opens (or creates) the store at `path` and brings its tables up to date.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Self, DatabaseError> {
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&connection)?;
        Ok(Self { connection: Mutex::new(connection) })
    }

    pub fn add_history(&self, connection_id: String, sql: String, elapsed_ms: Option<u64>, failed: bool) -> Result<(), DatabaseError> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO history (connection_id, sql, ran_at, elapsed_ms, failed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![connection_id, sql, now(), elapsed_ms.map(|ms| ms as i64), failed],
        )?;
        connection.execute(
            "DELETE FROM history WHERE connection_id = ?1 AND id NOT IN
               (SELECT id FROM history WHERE connection_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![connection_id, HISTORY_LIMIT],
        )?;
        Ok(())
    }

    /// Newest first. `search` matches anywhere in the SQL, ignoring case.
    pub fn history(&self, connection_id: String, search: String, limit: u32) -> Result<Vec<HistoryEntry>, DatabaseError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, sql, ran_at, elapsed_ms, failed FROM history
             WHERE connection_id = ?1 AND (?2 = '' OR instr(lower(sql), lower(?2)) > 0)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![connection_id, search, limit], |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                sql: row.get(1)?,
                ran_at: row.get(2)?,
                elapsed_ms: row.get::<_, Option<i64>>(3)?.map(|ms| ms.max(0) as u64),
                failed: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn clear_history(&self, connection_id: String) -> Result<(), DatabaseError> {
        self.lock()?.execute("DELETE FROM history WHERE connection_id = ?1", params![connection_id])?;
        Ok(())
    }

    /// Saves a new query, or updates the one with `id`.
    pub fn save_query(&self, connection_id: String, id: Option<i64>, name: String, sql: String) -> Result<SavedQuery, DatabaseError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(DatabaseError::new("A saved query needs a name."));
        }
        let connection = self.lock()?;
        let updated_at = now();
        let id = match id {
            Some(id) => {
                let changed = connection.execute(
                    "UPDATE saved_queries SET name = ?1, sql = ?2, updated_at = ?3 WHERE id = ?4",
                    params![name, sql, updated_at, id],
                )?;
                if changed == 0 {
                    return Err(DatabaseError::new("The saved query no longer exists."));
                }
                id
            }
            None => {
                connection.execute(
                    "INSERT INTO saved_queries (connection_id, name, sql, updated_at) VALUES (?1, ?2, ?3, ?4)",
                    params![connection_id, name, sql, updated_at],
                )?;
                connection.last_insert_rowid()
            }
        };
        Ok(SavedQuery { id, name, sql, updated_at })
    }

    pub fn rename_query(&self, id: i64, name: String) -> Result<(), DatabaseError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(DatabaseError::new("A saved query needs a name."));
        }
        self.lock()?.execute("UPDATE saved_queries SET name = ?1, updated_at = ?2 WHERE id = ?3", params![name, now(), id])?;
        Ok(())
    }

    pub fn delete_query(&self, id: i64) -> Result<(), DatabaseError> {
        self.lock()?.execute("DELETE FROM saved_queries WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// By name, ignoring case.
    pub fn saved_queries(&self, connection_id: String) -> Result<Vec<SavedQuery>, DatabaseError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, name, sql, updated_at FROM saved_queries WHERE connection_id = ?1 ORDER BY lower(name), id",
        )?;
        let rows = statement.query_map(params![connection_id], |row| {
            Ok(SavedQuery { id: row.get(0)?, name: row.get(1)?, sql: row.get(2)?, updated_at: row.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Everything a deleted connection left behind.
    pub fn forget_connection(&self, connection_id: String) -> Result<(), DatabaseError> {
        let connection = self.lock()?;
        connection.execute("DELETE FROM history WHERE connection_id = ?1", params![connection_id])?;
        connection.execute("DELETE FROM saved_queries WHERE connection_id = ?1", params![connection_id])?;
        Ok(())
    }
}

impl LocalStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, DatabaseError> {
        self.connection.lock().map_err(|_| DatabaseError::new("The local store is unavailable after an earlier failure."))
    }
}

/// Each step runs once, in order; `user_version` records how far a file got.
fn migrate(connection: &Connection) -> Result<(), DatabaseError> {
    const STEPS: &[&str] = &[
        "CREATE TABLE history (
             id INTEGER PRIMARY KEY,
             connection_id TEXT NOT NULL,
             sql TEXT NOT NULL,
             ran_at INTEGER NOT NULL,
             elapsed_ms INTEGER,
             failed INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX history_by_connection ON history (connection_id, id);
         CREATE TABLE saved_queries (
             id INTEGER PRIMARY KEY,
             connection_id TEXT NOT NULL,
             name TEXT NOT NULL,
             sql TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE INDEX saved_queries_by_connection ON saved_queries (connection_id);",
    ];
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).optional()?.unwrap_or(0);
    for (index, step) in STEPS.iter().enumerate().skip(version.max(0) as usize) {
        connection.execute_batch(&format!("BEGIN; {step} PRAGMA user_version = {}; COMMIT;", index + 1))?;
    }
    Ok(())
}

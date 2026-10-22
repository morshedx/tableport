//! SQLite: a database file on this Mac. rusqlite is synchronous, so each
//! call runs on a blocking thread of the core's runtime, one at a time.

use std::sync::{Arc, Mutex};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::session::ConnectionSettings;
use crate::DatabaseError;

#[derive(Clone)]
pub(crate) struct SqliteLink {
    /// None once closed.
    connection: Arc<Mutex<Option<Connection>>>,
    /// Stops the running statement from another thread (Cancel).
    interrupt: Arc<rusqlite::InterruptHandle>,
    pub(crate) version: String,
    name: String,
}

impl SqliteLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let path = settings.database.clone();
        let read_only = settings.read_only;
        let name = settings.endpoint();
        let connection = tokio::task::spawn_blocking(move || -> Result<Connection, DatabaseError> {
            // No CREATE flag: a mistyped path must not leave an empty file.
            let flags = if read_only {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            } | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let connection = Connection::open_with_flags(&path, flags).map_err(|error| sqlite_error(&error))?;
            // A read-only connection refuses writes even through ATTACH.
            if read_only {
                connection.execute_batch("PRAGMA query_only = ON").map_err(|error| sqlite_error(&error))?;
            }
            // Foreign keys are off by default in SQLite; the review step
            // relies on the database enforcing them.
            connection.execute_batch("PRAGMA foreign_keys = ON").map_err(|error| sqlite_error(&error))?;
            Ok(connection)
        })
        .await
        .map_err(|error| DatabaseError::new(format!("Could not open the file: {error}")))??;
        let interrupt = Arc::new(connection.get_interrupt_handle());
        Ok(Self {
            connection: Arc::new(Mutex::new(Some(connection))),
            interrupt,
            version: format!("SQLite {}", rusqlite::version()),
            name,
        })
    }

    /// Runs `work` with the connection on a blocking thread.
    pub(crate) async fn with<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, DatabaseError> + Send + 'static,
    ) -> Result<T, DatabaseError> {
        let connection = self.connection.clone();
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || {
            let guard = connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let connection = guard.as_ref().ok_or_else(|| DatabaseError::new(format!("{name} is closed.")))?;
            work(connection)
        })
        .await
        .map_err(|error| DatabaseError::new(format!("The SQLite call failed: {error}")))?
    }

    /// One statement's rows, every value as text (None for NULL).
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        self.with(move |connection| {
            let mut statement = connection.prepare(&sql).map_err(|error| sqlite_error(&error))?;
            let count = statement.column_count();
            let mut rows = statement.query([]).map_err(|error| sqlite_error(&error))?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    values.push(text(row.get_ref(index).map_err(|error| sqlite_error(&error))?));
                }
                out.push(values);
            }
            Ok(out)
        })
        .await
    }

    /// Runs statements separated by `;`, discarding rows.
    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        self.with(move |connection| connection.execute_batch(&sql).map_err(|error| sqlite_error(&error))).await
    }

    /// The running statement fails with "interrupted".
    pub(crate) fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.connection.lock().map(|guard| guard.is_none()).unwrap_or(true)
    }

    pub(crate) fn close(&self) {
        if let Ok(mut guard) = self.connection.lock() {
            guard.take();
        }
    }
}

/// A value as the grid shows it: numbers in their usual form, text as is,
/// a blob as `\x…` hex as PostgreSQL shows bytea.
pub(crate) fn text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(number.to_string()),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
    }
}

pub(crate) fn sqlite_error(error: &rusqlite::Error) -> DatabaseError {
    DatabaseError::new(error.to_string())
}

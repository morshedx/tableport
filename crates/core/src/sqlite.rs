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

    /// Runs `statements` in one transaction: all or none. Each of the first
    /// `checked` (updates and deletes by primary key) must change exactly
    /// one row; if one finds none, someone else changed or deleted it, and
    /// nothing is saved.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        self.with(move |connection| {
            let transaction = connection.unchecked_transaction().map_err(|error| sqlite_error(&error))?;
            for (index, statement) in statements.iter().enumerate() {
                let changed = transaction.execute(statement, []).map_err(|error| sqlite_error(&error))?;
                if index < checked && changed == 0 {
                    // Dropping the transaction rolls it back.
                    return Err(DatabaseError::new(
                        "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.",
                    ));
                }
            }
            transaction.commit().map_err(|error| sqlite_error(&error))
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

// MARK: - Catalog

use crate::rows::{quote_identifier, quote_literal, Column, ColumnKind};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::DatabaseSession;

impl DatabaseSession {
    /// `main`, `temp` when it has tables, and attached databases, each with
    /// its tables and views. SQLite's own tables are left out.
    pub(crate) async fn sqlite_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let names: Vec<String> = self
            .text_rows("SELECT name FROM pragma_database_list ORDER BY seq".into())
            .await?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect();
        let mut schemas = Vec::new();
        for name in names {
            let rows = self
                .text_rows(format!(
                    "SELECT name, type FROM {}.sqlite_master \
                     WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
                    quote_identifier(&name)
                ))
                .await?;
            let relations: Vec<Relation> = rows
                .into_iter()
                .filter_map(|row| {
                    let [Some(table), Some(kind)] = <[Option<String>; 2]>::try_from(row).ok()? else { return None };
                    let kind = if kind == "view" { RelationKind::View } else { RelationKind::Table };
                    Some(Relation { schema: name.clone(), name: table, kind })
                })
                .collect();
            if name == "temp" && relations.is_empty() {
                continue;
            }
            schemas.push(SchemaContents { name, relations });
        }
        Ok(schemas)
    }

    /// Columns from `pragma_table_info`, typed by SQLite's affinity rules:
    /// the declared type is free text, and its words decide.
    pub(crate) async fn sqlite_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT name, type FROM pragma_table_info({}, {}) ORDER BY cid",
                quote_literal(&relation.name)?,
                quote_literal(&relation.schema)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), column_type] = <[Option<String>; 2]>::try_from(row).ok()? else { return None };
                let column_type = column_type.unwrap_or_default();
                let upper = column_type.to_uppercase();
                let is_numeric = ["INT", "REAL", "FLOA", "DOUB", "NUMERIC", "DECIMAL"].iter().any(|word| upper.contains(word));
                // Dates are text in SQLite; the date picker writes the forms
                // SQLite's date functions read.
                let kind = if upper.contains("TIMESTAMP") || upper.contains("DATETIME") {
                    ColumnKind::Timestamp
                } else if upper.contains("DATE") {
                    ColumnKind::Date
                } else if upper.contains("TIME") {
                    ColumnKind::Time
                } else {
                    ColumnKind::Other
                };
                Some(Column { name, column_type, kind, enum_values: Vec::new(), is_numeric })
            })
            .collect())
    }

    pub(crate) async fn sqlite_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT name FROM pragma_table_info({}, {}) WHERE pk > 0 ORDER BY pk",
                quote_literal(&relation.name)?,
                quote_literal(&relation.schema)?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Single-column foreign keys that name the referenced column, as for
    /// PostgreSQL: following a key filters the other table by one column.
    pub(crate) async fn sqlite_foreign_keys(&self, relation: &Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        let list = format!("pragma_foreign_key_list({}, {})", quote_literal(&relation.name)?, quote_literal(&relation.schema)?);
        let rows = self
            .text_rows(format!(
                "SELECT f.\"from\", f.\"table\", f.\"to\" FROM {list} f \
                 WHERE f.\"to\" IS NOT NULL AND (SELECT count(*) FROM {list} g WHERE g.id = f.id) = 1 ORDER BY f.id"
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(column), Some(table), Some(referenced_column)] = <[Option<String>; 3]>::try_from(row).ok()? else {
                    return None;
                };
                let referenced_relation = Relation { schema: relation.schema.clone(), name: table, kind: RelationKind::Table };
                Some(ForeignKey { column, referenced_relation, referenced_column })
            })
            .collect())
    }
}

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

    /// A table rebuild: its statements in one transaction with foreign keys
    /// off, as SQLite's recipe asks, then a check that every foreign key of
    /// the checked tables still finds its row. Foreign keys can only be
    /// switched outside a transaction. Legacy ALTER TABLE keeps the rename
    /// from rewriting, or refusing over, views and triggers that name the
    /// table while it is briefly gone.
    pub(crate) async fn rebuild(&self, rebuild: crate::sqlite_rebuild::Rebuild, schema: String) -> Result<(), DatabaseError> {
        self.with(move |connection| {
            let foreign_keys: i64 =
                connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).map_err(|error| sqlite_error(&error))?;
            connection
                .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON;")
                .map_err(|error| sqlite_error(&error))?;
            let result = (|| {
                let transaction = connection.unchecked_transaction().map_err(|error| sqlite_error(&error))?;
                for statement in &rebuild.statements {
                    transaction.execute_batch(statement).map_err(|error| sqlite_error(&error))?;
                }
                for table in &rebuild.checked {
                    let violations: i64 = transaction
                        .query_row(
                            &format!(
                                "SELECT count(*) FROM pragma_foreign_key_check({}, {})",
                                crate::rows::quote_literal(table)?,
                                crate::rows::quote_literal(&schema)?
                            ),
                            [],
                            |row| row.get(0),
                        )
                        .map_err(|error| sqlite_error(&error))?;
                    if violations > 0 {
                        return Err(DatabaseError::new(format!(
                            "{violations} rows of {table} would point at rows that don't exist. Nothing was changed."
                        )));
                    }
                }
                transaction.commit().map_err(|error| sqlite_error(&error))
            })();
            let restore = connection
                .execute_batch(&format!(
                    "PRAGMA legacy_alter_table = OFF; PRAGMA foreign_keys = {};",
                    if foreign_keys == 1 { "ON" } else { "OFF" }
                ))
                .map_err(|error| sqlite_error(&error));
            result.and(restore).map_err(|error| {
                if error.message().contains("foreign key mismatch") {
                    DatabaseError::new(format!(
                        "SQLite needs the referenced column to be the primary key or to have a unique index. ({})",
                        error.message()
                    ))
                } else {
                    error
                }
            })
        })
        .await
    }

    /// The query editor's Run: each statement in turn, as one transaction
    /// unless the SQL manages its own (BEGIN, COMMIT…). Keeps the first
    /// `max_rows` rows of each result and counts the rest.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{cut, QueryResult, ResultSet};
        let statements = crate::sql_text::statements(&sql);
        let verbs = crate::sql_text::statement_verbs(&sql);
        let manages_transaction =
            verbs.iter().any(|verb| matches!(verb.as_str(), "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"));
        self.with(move |connection| {
            let start = std::time::Instant::now();
            let transaction = if manages_transaction {
                None
            } else {
                Some(connection.unchecked_transaction().map_err(|error| sqlite_error(&error))?)
            };
            let mut result_sets = Vec::new();
            let mut statement_counts = Vec::new();
            for source in &statements {
                let mut statement = connection.prepare(source).map_err(|error| sqlite_error(&error))?;
                let count = statement.column_count();
                if count == 0 {
                    statement.raw_execute().map_err(|error| sqlite_error(&error))?;
                    statement_counts.push(connection.changes());
                    continue;
                }
                let columns: Vec<String> = statement.column_names().into_iter().map(str::to_string).collect();
                // Declared types first; for expressions, the first value's type.
                let mut numeric: Vec<Option<bool>> = statement
                    .columns()
                    .iter()
                    .map(|column| column.decl_type().map(|declared| is_numeric_type(declared)))
                    .collect();
                let mut rows = statement.query([]).map_err(|error| sqlite_error(&error))?;
                let mut kept = Vec::new();
                let mut total = 0u64;
                while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
                    if total < max_rows {
                        let mut values = Vec::with_capacity(count);
                        for index in 0..count {
                            let value = row.get_ref(index).map_err(|error| sqlite_error(&error))?;
                            if numeric[index].is_none() && !matches!(value, ValueRef::Null) {
                                numeric[index] = Some(matches!(value, ValueRef::Integer(_) | ValueRef::Real(_)));
                            }
                            values.push(text(value).as_deref().map(cut));
                        }
                        kept.push(values);
                    }
                    total += 1;
                }
                statement_counts.push(total);
                result_sets.push(ResultSet {
                    columns,
                    rows: kept,
                    total_rows: total,
                    numeric_columns: numeric.into_iter().map(|is| is.unwrap_or(false)).collect(),
                });
            }
            if let Some(transaction) = transaction {
                transaction.commit().map_err(|error| sqlite_error(&error))?;
            }
            Ok(QueryResult {
                result_sets,
                statement_counts,
                statement_verbs: verbs,
                elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            })
        })
        .await
    }

    /// Writes the rows of `select` to `path` as CSV, JSON or INSERT
    /// statements, a row at a time, and returns how many it wrote.
    pub(crate) async fn export(
        &self,
        select: String,
        columns: Vec<Column>,
        relation: Relation,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::ExportFormat;
        use std::io::Write;
        self.with(move |connection| {
            let file = std::fs::File::create(&path).map_err(|error| DatabaseError::new(format!("Can't create {path}: {error}.")))?;
            let mut out = std::io::BufWriter::new(file);
            let write_error = |error: std::io::Error| DatabaseError::new(format!("Can't write {path}: {error}."));
            let mut statement = connection.prepare(&select).map_err(|error| sqlite_error(&error))?;
            let names: Vec<String> = statement.column_names().into_iter().map(str::to_string).collect();
            let mut rows = statement.query([]).map_err(|error| sqlite_error(&error))?;
            let target = relation.quoted_name();
            let quoted_names = if columns.is_empty() { names.clone() } else { columns.iter().map(|c| c.name.clone()).collect() }
                .iter()
                .map(|name| quote_identifier(name))
                .collect::<Vec<_>>()
                .join(", ");
            let mut count = 0u64;
            if format == ExportFormat::Csv {
                writeln!(out, "{}", names.iter().map(|name| csv_field(name)).collect::<Vec<_>>().join(",")).map_err(write_error)?;
            }
            if format == ExportFormat::Json {
                out.write_all(b"[").map_err(write_error)?;
            }
            while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
                let values = (0..names.len())
                    .map(|index| row.get_ref(index).map_err(|error| sqlite_error(&error)))
                    .collect::<Result<Vec<_>, _>>()?;
                match format {
                    ExportFormat::Csv => {
                        let line = values.iter().map(|value| text(*value).as_deref().map(csv_field).unwrap_or_default()).collect::<Vec<_>>();
                        writeln!(out, "{}", line.join(",")).map_err(write_error)?;
                    }
                    ExportFormat::Json => {
                        let object: serde_json::Map<String, serde_json::Value> =
                            names.iter().cloned().zip(values.iter().map(|value| json(*value))).collect();
                        let separator = if count == 0 { "\n  " } else { ",\n  " };
                        write!(out, "{separator}{}", serde_json::Value::Object(object)).map_err(write_error)?;
                    }
                    ExportFormat::Sql => {
                        let literals = values
                            .iter()
                            .map(|value| match value {
                                ValueRef::Null => Ok("NULL".to_string()),
                                ValueRef::Integer(number) => Ok(number.to_string()),
                                ValueRef::Real(number) => Ok(real(*number)),
                                ValueRef::Blob(bytes) => {
                                    Ok(format!("X'{}'", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()))
                                }
                                ValueRef::Text(_) => quote_literal(&text(*value).unwrap_or_default()),
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        writeln!(out, "INSERT INTO {target} ({quoted_names}) VALUES ({});", literals.join(", ")).map_err(write_error)?;
                    }
                }
                count += 1;
            }
            if format == ExportFormat::Json {
                out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).map_err(write_error)?;
            }
            out.flush().map_err(write_error)?;
            Ok(count)
        })
        .await
    }

    /// One statement's rows as JSON objects, with the stored types, for AI
    /// access.
    pub(crate) async fn json_rows(&self, sql: String) -> Result<Vec<serde_json::Value>, DatabaseError> {
        self.with(move |connection| {
            let mut statement = connection.prepare(&sql).map_err(|error| sqlite_error(&error))?;
            let names: Vec<String> = statement.column_names().into_iter().map(str::to_string).collect();
            let mut rows = statement.query([]).map_err(|error| sqlite_error(&error))?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
                let mut object = serde_json::Map::new();
                for (index, name) in names.iter().enumerate() {
                    object.insert(name.clone(), json(row.get_ref(index).map_err(|error| sqlite_error(&error))?));
                }
                out.push(serde_json::Value::Object(object));
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
        ValueRef::Real(number) => Some(real(number)),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
    }
}

/// A real as SQLite writes it: whole numbers keep `.0` (`1.0`, not `1`),
/// so the grid, query results and exports agree with SQLite's own CAST.
pub(crate) fn real(number: f64) -> String {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{number:.1}")
    } else {
        number.to_string()
    }
}

/// Whether SQLite's affinity rules give a declared type a numeric affinity.
pub(crate) fn is_numeric_type(declared: &str) -> bool {
    let upper = declared.to_uppercase();
    ["INT", "REAL", "FLOA", "DOUB", "NUMERIC", "DECIMAL"].iter().any(|word| upper.contains(word))
}

/// A JSON value with the stored type: numbers stay numbers.
fn json(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(number) => serde_json::Value::from(number),
        ValueRef::Real(number) => serde_json::Number::from_f64(number).map_or(serde_json::Value::Null, serde_json::Value::Number),
        _ => text(value).map_or(serde_json::Value::Null, serde_json::Value::String),
    }
}

/// RFC 4180 quoting, as the clipboard's CSV.
pub(crate) fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
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
        // D1 has one database, and refuses pragmas outside its own list.
        let is_d1 = self.engine() == crate::Engine::D1;
        let names: Vec<String> = if is_d1 {
            vec!["main".into()]
        } else {
            self.text_rows("SELECT name FROM pragma_database_list ORDER BY seq".into())
                .await?
                .into_iter()
                .filter_map(|row| row.into_iter().next().flatten())
                .collect()
        };
        // D1 keeps its own tables, named _cf_…, beside the user's.
        let hidden = if is_d1 { " AND name NOT LIKE '\\_cf\\_%' ESCAPE '\\'" } else { "" };
        let mut schemas = Vec::new();
        for name in names {
            let rows = self
                .text_rows(format!(
                    "SELECT name, type FROM {}.sqlite_master \
                     WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'{hidden} ORDER BY name",
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
                let is_numeric = is_numeric_type(&column_type);
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

// MARK: - Structure

use crate::structure::{ColumnDetail, ConstraintDetail, ConstraintKind, IndexDetail, TableStructure};

impl DatabaseSession {
    /// Columns, indexes and constraints. SQLite constraints rarely have
    /// names, so they get descriptive ones ("primary key", "foreign key 1").
    pub(crate) async fn sqlite_structure(&self, relation: &Relation) -> Result<TableStructure, DatabaseError> {
        let table = quote_literal(&relation.name)?;
        let schema = quote_literal(&relation.schema)?;
        let column_rows = self
            .text_rows(format!("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info({table}, {schema}) ORDER BY cid"))
            .await?;
        let mut key: Vec<(i64, String)> = Vec::new();
        let columns = column_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), column_type, Some(not_null), default_value, Some(pk)] = <[Option<String>; 5]>::try_from(row).ok()? else {
                    return None;
                };
                if let Ok(position) = pk.parse::<i64>() {
                    if position > 0 {
                        key.push((position, name.clone()));
                    }
                }
                Some(ColumnDetail {
                    name,
                    column_type: column_type.unwrap_or_default(),
                    // A primary key column is never NULL, though SQLite only
                    // enforces it for INTEGER keys.
                    nullable: not_null == "0" && pk == "0",
                    default_value,
                    comment: None,
                })
            })
            .collect();
        key.sort();

        let index_rows = self
            .text_rows(format!(
                "SELECT l.name, l.\"unique\", l.origin, m.sql, \
                        (SELECT group_concat(name, ', ') FROM pragma_index_info(l.name, {schema})) \
                 FROM pragma_index_list({table}, {schema}) l \
                 LEFT JOIN {}.sqlite_master m ON m.type = 'index' AND m.name = l.name ORDER BY l.origin <> 'pk', l.name",
                quote_identifier(&relation.schema)
            ))
            .await?;
        let mut constraints = Vec::new();
        if !key.is_empty() {
            let names = key.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>().join(", ");
            constraints.push(ConstraintDetail {
                name: "primary key".into(),
                kind: ConstraintKind::PrimaryKey,
                definition: format!("PRIMARY KEY ({names})"),
            });
        }
        let mut indexes = Vec::new();
        for row in index_rows {
            let [Some(name), Some(unique), Some(origin), sql, columns] = <[Option<String>; 5]>::try_from(row).unwrap_or_default() else {
                continue;
            };
            let columns = columns.unwrap_or_default();
            if origin == "u" {
                constraints.push(ConstraintDetail {
                    name: name.clone(),
                    kind: ConstraintKind::Unique,
                    definition: format!("UNIQUE ({columns})"),
                });
            }
            indexes.push(IndexDetail {
                // Automatic indexes have no SQL of their own.
                definition: sql.unwrap_or_else(|| format!("automatic index on ({columns})")),
                name,
                is_unique: unique == "1",
                is_primary: origin == "pk",
            });
        }

        let key_rows = self
            .text_rows(format!(
                "SELECT id, \"from\", \"table\", \"to\", on_update, on_delete FROM pragma_foreign_key_list({table}, {schema}) ORDER BY id, seq"
            ))
            .await?;
        let mut keys: Vec<(String, Vec<String>, String, Vec<String>, String)> = Vec::new();
        for row in key_rows {
            let [Some(id), Some(from), Some(referenced), to, _, Some(on_delete)] = <[Option<String>; 6]>::try_from(row).unwrap_or_default() else {
                continue;
            };
            match keys.last_mut().filter(|entry| entry.0 == id) {
                Some(entry) => {
                    entry.1.push(from);
                    entry.3.push(to.unwrap_or_default());
                }
                None => keys.push((id, vec![from], referenced, vec![to.unwrap_or_default()], on_delete)),
            }
        }
        // Names given with CONSTRAINT; the others are numbered in SQLite's order.
        let definition = self
            .text_rows(format!(
                "SELECT sql FROM {}.sqlite_master WHERE type = 'table' AND name = {table}",
                quote_identifier(&relation.schema)
            ))
            .await?
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten())
            .unwrap_or_default();
        let mut names = crate::sqlite_rebuild::foreign_key_names(&definition);
        for (index, (_, from, referenced, to, on_delete)) in keys.into_iter().enumerate() {
            let same = |columns: &[String]| {
                columns.len() == from.len() && columns.iter().zip(&from).all(|(column, from)| column.eq_ignore_ascii_case(from))
            };
            let name = names
                .iter()
                .position(|(columns, table, _)| same(columns) && table.eq_ignore_ascii_case(&referenced))
                .map(|position| names.remove(position).2)
                .unwrap_or_else(|| format!("foreign key {}", index + 1));
            let target = if to.iter().all(String::is_empty) { String::new() } else { format!("({})", to.join(", ")) };
            let action = if on_delete == "NO ACTION" { String::new() } else { format!(" ON DELETE {on_delete}") };
            constraints.push(ConstraintDetail {
                name,
                kind: ConstraintKind::ForeignKey,
                definition: format!("FOREIGN KEY ({}) REFERENCES {referenced}{target}{action}", from.join(", ")),
            });
        }
        Ok(TableStructure { columns, indexes, constraints })
    }
}

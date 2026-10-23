//! Oracle Database, through ODPI-C and Oracle's Instant Client, which the
//! user installs and TablePort loads from its folder at run time
//! ([`load_oracle_client`]). Schemas are Oracle users; a connection is to one
//! service (a pluggable database), so there is one database per connection.
//!
//! The driver blocks, so each call runs on Tokio's blocking pool. One session
//! holds two connections, as for the other engines: TablePort's own SQL on
//! one, the query editor's on the other, which Cancel can break from another
//! thread. Both set the NLS date formats, so dates TablePort shows as text
//! read back the same way in filters and edits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oracle::sql_type::{OracleType, Timestamp};
use oracle::{Connection, Connector};

use crate::rows::{quote_identifier, quote_literal, Column, ColumnKind};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::structure::{non_empty, ColumnDetail, ConstraintDetail, ConstraintKind, IndexDetail, StructureChange, TableStructure};
use crate::{DatabaseError, DatabaseSession};

const SETUP: &[&str] = &[
    "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS'",
    "ALTER SESSION SET NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF'",
    "ALTER SESSION SET NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'",
    "ALTER SESSION SET NLS_NUMERIC_CHARACTERS = '.,'",
];

const ROW_CHANGED: &str = "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.";

/// Loads Oracle's Instant Client from `directory`, once for the app's life,
/// and returns its version, such as "23.3.0.23.9". Loading another folder
/// later needs a restart of TablePort: the client stays loaded.
#[uniffi::export]
pub fn load_oracle_client(directory: String) -> Result<String, DatabaseError> {
    let mut params = oracle::InitParams::new();
    params.oracle_client_lib_dir(directory.trim()).map_err(|error| DatabaseError::new(error.to_string()))?;
    params.init().map_err(|error| {
        DatabaseError::new(format!(
            "Instant Client couldn't be loaded from {directory}: {}",
            first_line(&error.to_string())
        ))
    })?;
    oracle::Version::client().map(|version| version.to_string()).map_err(|error| DatabaseError::new(first_line(&error.to_string())))
}

#[derive(Clone)]
pub(crate) struct OracleLink {
    internal: Arc<Mutex<Connection>>,
    /// Shared so Cancel can break it while a query holds `editor_lock`.
    editor: Arc<Connection>,
    editor_lock: Arc<tokio::sync::Mutex<()>>,
    /// "Oracle 23.5.0.24.7".
    pub(crate) version: String,
    read_only: bool,
    closed: Arc<AtomicBool>,
    endpoint: String,
    /// The schema of the login, where unqualified names are found.
    pub(crate) user: String,
    _tunnel: Option<Arc<Tunnel>>,
}

/// The rows of one query with its columns' names and types.
struct Rows {
    names: Vec<String>,
    types: Vec<OracleType>,
    rows: Vec<Vec<Option<String>>>,
    total: u64,
}

impl OracleLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        let tunnel = match &settings.ssh {
            Some(ssh) if !settings.host.trim_start().starts_with('(') => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            Some(_) => return Err(DatabaseError::new("A full connect descriptor can't go through an SSH tunnel; give the host, port and service instead.")),
            None => None,
        };
        let connect_string = connect_string(settings, tunnel.as_ref().map(|tunnel| tunnel.local_port));
        let (user, password) = (settings.user.clone(), settings.password.clone().unwrap_or_default());
        let open = move || -> Result<Connection, oracle::Error> {
            let mut connector = Connector::new(user.as_str(), password.as_str(), connect_string.as_str());
            connector.driver_name("TablePort");
            let conn = connector.connect()?;
            for statement in SETUP {
                conn.execute(statement, &[])?;
            }
            Ok(conn)
        };
        let (internal, editor) = tokio::time::timeout(Duration::from_secs(20), async {
            let first = tokio::task::spawn_blocking(open.clone());
            let second = tokio::task::spawn_blocking(open);
            let internal = first.await.map_err(|error| DatabaseError::new(error.to_string()))?;
            let editor = second.await.map_err(|error| DatabaseError::new(error.to_string()))?;
            Ok::<_, DatabaseError>((internal, editor))
        })
        .await
        .map_err(|_| DatabaseError::new(format!("{endpoint} did not answer within 20 seconds.")))??;
        let internal = internal.map_err(|error| DatabaseError::from_oracle(&error, &endpoint))?;
        let editor = editor.map_err(|error| DatabaseError::from_oracle(&error, &endpoint))?;
        let (version, _banner) = internal.server_version().map_err(|error| DatabaseError::from_oracle(&error, &endpoint))?;
        let user = internal.current_schema().unwrap_or_else(|_| settings.user.to_uppercase());
        Ok(Self {
            internal: Arc::new(Mutex::new(internal)),
            editor: Arc::new(editor),
            editor_lock: Arc::new(tokio::sync::Mutex::new(())),
            version: format!("Oracle {version}"),
            read_only: settings.read_only,
            closed: Arc::new(AtomicBool::new(false)),
            endpoint,
            user,
            _tunnel: tunnel,
        })
    }

    fn fail(&self, error: &oracle::Error) -> DatabaseError {
        // ORA-03113 and ORA-03114: the connection is gone.
        if error.db_error().is_some_and(|db| matches!(db.code(), 3113 | 3114 | 3135 | 28)) {
            self.closed.store(true, Ordering::Relaxed);
        }
        DatabaseError::from_oracle(error, &self.endpoint)
    }

    fn check_open(&self) -> Result<(), DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(())
    }

    fn refuse_writes(&self) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        Ok(())
    }

    /// Runs `work` with TablePort's own connection, on the blocking pool.
    async fn with_internal<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, oracle::Error> + Send + 'static,
    ) -> Result<T, DatabaseError> {
        self.check_open()?;
        let link = self.clone();
        tokio::task::spawn_blocking(move || {
            let conn = link.internal.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&conn).map_err(|error| link.fail(&error))
        })
        .await
        .map_err(|error| DatabaseError::new(error.to_string()))?
    }

    /// Every row of `sql`, each value as text (None for NULL).
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        self.with_internal(move |conn| read_rows(conn, &sql, u64::MAX).map(|rows| rows.rows)).await
    }

    /// Runs a script and keeps nothing: each statement in turn, then COMMIT.
    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        self.with_internal(move |conn| {
            for statement in split_script(&sql) {
                if let Err(error) = conn.execute(&statement, &[]) {
                    conn.rollback().ok();
                    return Err(error);
                }
            }
            conn.commit()
        })
        .await
    }

    /// Runs `statements` as one transaction: all or none. Each of the first
    /// `checked` (updates and deletes by primary key) must find its row.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let changed = Arc::new(AtomicBool::new(false));
        let outcome = {
            let changed = changed.clone();
            self.with_internal(move |conn| {
                for (index, statement) in statements.iter().enumerate() {
                    let found = match conn.execute(statement, &[]) {
                        Ok(done) => done.row_count()?,
                        Err(error) => {
                            conn.rollback().ok();
                            return Err(error);
                        }
                    };
                    if index < checked && found == 0 {
                        changed.store(true, Ordering::Relaxed);
                        return conn.rollback();
                    }
                }
                conn.commit()
            })
            .await
        };
        outcome?;
        if changed.load(Ordering::Relaxed) {
            return Err(DatabaseError::new(ROW_CHANGED));
        }
        Ok(())
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        match tokio::time::timeout(Duration::from_secs(5), self.with_internal(|conn| conn.ping())).await {
            Ok(result) => result,
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    /// Stops the editor's running statement.
    pub(crate) fn cancel(&self) -> Result<(), DatabaseError> {
        self.editor.break_execution().map_err(|error| self.fail(&error))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// The query editor's Run: statements in turn, PL/SQL blocks whole. It
    /// commits at the end unless the script commits or rolls back itself;
    /// DDL commits on its own, as Oracle always does.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{QueryResult, ResultSet};
        self.check_open()?;
        let statements = split_script(&sql);
        let verbs: Vec<String> = statements.iter().map(|statement| first_word(statement)).collect();
        let manages_transaction = verbs.iter().any(|verb| matches!(verb.as_str(), "COMMIT" | "ROLLBACK" | "SAVEPOINT"));
        if self.read_only {
            if let Some(verb) = verbs.iter().find(|verb| !READ_VERBS.contains(&verb.as_str())) {
                return Err(DatabaseError::new(format!("This connection is read-only. {verb} was not run.")));
            }
        }
        let _guard = self.editor_lock.lock().await;
        let link = self.clone();
        let read_only = self.read_only;
        tokio::task::spawn_blocking(move || {
            let conn = &link.editor;
            let start = std::time::Instant::now();
            if read_only {
                conn.execute("SET TRANSACTION READ ONLY", &[]).map_err(|error| link.fail(&error))?;
            }
            let mut result_sets = Vec::new();
            let mut counts = Vec::new();
            let outcome: Result<(), oracle::Error> = (|| {
                for statement in &statements {
                    let mut prepared = conn.statement(statement).build()?;
                    if prepared.is_query() {
                        let rows = read_prepared(&mut prepared, max_rows)?;
                        counts.push(rows.total);
                        result_sets.push(ResultSet {
                            columns: rows.names,
                            rows: rows.rows.into_iter().map(|row| row.into_iter().map(|cell| cell.map(|text| crate::query::cut(&text))).collect()).collect(),
                            total_rows: rows.total,
                            numeric_columns: rows.types.iter().map(is_numeric_type_of).collect(),
                        });
                    } else {
                        prepared.execute(&[])?;
                        counts.push(if prepared.is_dml() { prepared.row_count()? } else { 0 });
                    }
                }
                Ok(())
            })();
            let ended = match (&outcome, manages_transaction, read_only) {
                (Ok(()), false, false) => conn.commit(),
                (Ok(()), true, false) => Ok(()),
                _ => conn.rollback(),
            };
            outcome.map_err(|error| link.fail(&error))?;
            ended.map_err(|error| link.fail(&error))?;
            Ok(QueryResult {
                result_sets,
                statement_counts: counts,
                statement_verbs: verbs,
                elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            })
        })
        .await
        .map_err(|error| DatabaseError::new(error.to_string()))?
    }

    /// One query for AI access: a subquery of a SELECT, so a second statement
    /// is a syntax error, in a READ ONLY transaction, which Oracle enforces,
    /// with a call timeout. Always rolled back.
    pub(crate) async fn read_only_json(&self, sql: &str, max_rows: u64, timeout: Duration) -> Result<Vec<serde_json::Value>, DatabaseError> {
        self.check_open()?;
        let verb = first_word(sql);
        if !matches!(verb.as_str(), "SELECT" | "WITH") {
            return Err(DatabaseError::new(format!("Only a SELECT or WITH query may run here, not {verb}.")));
        }
        let wrapped = format!("SELECT * FROM (\n{sql}\n) FETCH FIRST {} ROWS ONLY", max_rows + 1);
        let _guard = self.editor_lock.lock().await;
        let link = self.clone();
        tokio::task::spawn_blocking(move || {
            let conn = &link.editor;
            conn.set_call_timeout(Some(timeout)).map_err(|error| link.fail(&error))?;
            let outcome = conn.execute("SET TRANSACTION READ ONLY", &[]).and_then(|_| read_rows(conn, &wrapped, max_rows + 1));
            conn.rollback().ok();
            conn.set_call_timeout(None).ok();
            let rows = outcome.map_err(|error| {
                // ORA-03156: the call timed out.
                if error.db_error().is_some_and(|db| db.code() == 3156) {
                    DatabaseError::new(format!("The query ran longer than {} seconds and was stopped.", timeout.as_secs()))
                } else {
                    link.fail(&error)
                }
            })?;
            Ok(rows
                .rows
                .into_iter()
                .map(|row| {
                    let object: serde_json::Map<String, serde_json::Value> = rows
                        .names
                        .iter()
                        .cloned()
                        .zip(row.into_iter().zip(&rows.types).map(|(cell, oracle_type)| json(cell, oracle_type)))
                        .collect();
                    serde_json::Value::Object(object)
                })
                .collect())
        })
        .await
        .map_err(|error| DatabaseError::new(error.to_string()))?
    }

    /// Writes the rows of `select` to `path` and returns how many it wrote.
    pub(crate) async fn export(
        &self,
        select: String,
        relation: Relation,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::ExportFormat;
        use std::io::Write;
        self.with_internal(move |conn| {
            let write_error = |error: std::io::Error| oracle::Error::new(oracle::ErrorKind::InternalError, format!("Can't write {path}: {error}."));
            let file = std::fs::File::create(&path).map_err(write_error)?;
            let mut out = std::io::BufWriter::new(file);
            let mut prepared = conn.statement(&select).fetch_array_size(500).build()?;
            let rows = prepared.query(&[])?;
            let infos = rows.column_info().to_vec();
            let names: Vec<String> = infos.iter().map(|info| info.name().to_string()).collect();
            let types: Vec<OracleType> = infos.iter().map(|info| info.oracle_type().clone()).collect();
            let target = relation.quoted_name();
            let quoted_names = names.iter().map(|name| quote_identifier(name)).collect::<Vec<_>>().join(", ");
            match format {
                ExportFormat::Csv => writeln!(out, "{}", names.iter().map(|name| crate::sqlite::csv_field(name)).collect::<Vec<_>>().join(",")).map_err(write_error)?,
                ExportFormat::Json => write!(out, "[").map_err(write_error)?,
                ExportFormat::Sql => {}
            }
            let mut count = 0u64;
            for row in rows {
                let row = row?;
                let cells: Vec<Option<String>> = row.sql_values().iter().zip(&types).map(|(value, oracle_type)| value_text(value, oracle_type)).collect();
                let line = match format {
                    ExportFormat::Csv => cells.iter().map(|cell| cell.as_deref().map(crate::sqlite::csv_field).unwrap_or_default()).collect::<Vec<_>>().join(",") + "\n",
                    ExportFormat::Json => {
                        let object: serde_json::Map<String, serde_json::Value> =
                            names.iter().cloned().zip(cells.into_iter().zip(&types).map(|(cell, oracle_type)| json(cell, oracle_type))).collect();
                        format!("{}{}", if count == 0 { "\n  " } else { ",\n  " }, serde_json::Value::Object(object))
                    }
                    ExportFormat::Sql => {
                        let literals = cells.into_iter().zip(&types).map(|(cell, oracle_type)| literal(cell, oracle_type)).collect::<Vec<_>>();
                        format!("INSERT INTO {target} ({quoted_names}) VALUES ({});\n", literals.join(", "))
                    }
                };
                out.write_all(line.as_bytes()).map_err(write_error)?;
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
}

/// Statements a read-only connection may run.
const READ_VERBS: &[&str] = &["SELECT", "WITH", "DESCRIBE", "DESC", "EXPLAIN"];

/// Easy Connect: tcps://host:port/service, or the descriptor the user gave.
fn connect_string(settings: &ConnectionSettings, tunnel_port: Option<u16>) -> String {
    let host = settings.host.trim();
    // A whole descriptor, as the Oracle Cloud console shows it: (description=…).
    if host.starts_with('(') {
        return host.to_string();
    }
    let (host, port) = match tunnel_port {
        Some(port) => ("127.0.0.1", port),
        None => (host, settings.port),
    };
    let service = settings.database.trim();
    match settings.tls {
        TlsMode::Disable | TlsMode::Prefer => format!("tcp://{host}:{port}/{service}"),
        TlsMode::Require => format!("tcps://{host}:{port}/{service}?ssl_server_dn_match=no"),
        TlsMode::VerifyFull => format!("tcps://{host}:{port}/{service}?ssl_server_dn_match=yes"),
    }
}

fn read_rows(conn: &Connection, sql: &str, max_rows: u64) -> Result<Rows, oracle::Error> {
    let mut prepared = conn.statement(sql).fetch_array_size(500).build()?;
    read_prepared(&mut prepared, max_rows)
}

fn read_prepared(prepared: &mut oracle::Statement, max_rows: u64) -> Result<Rows, oracle::Error> {
    let rows = prepared.query(&[])?;
    let infos = rows.column_info().to_vec();
    let types: Vec<OracleType> = infos.iter().map(|info| info.oracle_type().clone()).collect();
    let mut kept = Vec::new();
    let mut total = 0u64;
    for row in rows {
        let row = row?;
        if total < max_rows {
            kept.push(row.sql_values().iter().zip(&types).map(|(value, oracle_type)| value_text(value, oracle_type)).collect());
        }
        total += 1;
    }
    Ok(Rows { names: infos.iter().map(|info| info.name().to_string()).collect(), types, rows: kept, total })
}

/// A value as the grid shows it: dates as the NLS formats TablePort sets,
/// bytes as `\x…` hex, objects by their type name.
fn value_text(value: &oracle::SqlValue, oracle_type: &OracleType) -> Option<String> {
    if value.is_null().unwrap_or(true) {
        return None;
    }
    match oracle_type {
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => value.get::<Vec<u8>>().ok().map(|bytes| hex(&bytes)),
        OracleType::Date => value.get::<Timestamp>().ok().map(|time| date_text(&time, false, false)),
        OracleType::Timestamp(_) => value.get::<Timestamp>().ok().map(|time| date_text(&time, true, false)),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => value.get::<Timestamp>().ok().map(|time| date_text(&time, true, true)),
        OracleType::Object(object) => Some(format!("({}.{})", object.schema(), object.name())),
        _ => Some(value.get::<String>().unwrap_or_else(|error| format!("(unreadable: {})", first_line(&error.to_string())))),
    }
}

/// "2024-01-02 03:04:05", with the fraction when there is one, and the
/// offset for zoned timestamps.
fn date_text(time: &Timestamp, fraction: bool, zone: bool) -> String {
    let mut text = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.year(),
        time.month(),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    );
    if fraction && time.nanosecond() != 0 {
        text += &format!(".{}", format!("{:09}", time.nanosecond()).trim_end_matches('0'));
    }
    if zone {
        let (hours, minutes) = (time.tz_hour_offset(), time.tz_minute_offset());
        text += &format!(" {}{:02}:{:02}", if hours < 0 || minutes < 0 { '-' } else { '+' }, hours.abs(), minutes.abs());
    }
    text
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn is_numeric_type_of(oracle_type: &OracleType) -> bool {
    matches!(oracle_type, OracleType::Number(_, _) | OracleType::Float(_) | OracleType::BinaryFloat | OracleType::BinaryDouble | OracleType::Int64 | OracleType::UInt64)
}

fn json(cell: Option<String>, oracle_type: &OracleType) -> serde_json::Value {
    let Some(text) = cell else { return serde_json::Value::Null };
    if is_numeric_type_of(oracle_type) {
        if let Ok(number) = text.parse::<serde_json::Number>() {
            return serde_json::Value::Number(number);
        }
    }
    if matches!(oracle_type, OracleType::Boolean) {
        return serde_json::Value::Bool(text.eq_ignore_ascii_case("true"));
    }
    serde_json::Value::String(text)
}

/// A value as an Oracle literal, for the SQL export: dates with TO_DATE
/// and TO_TIMESTAMP, bytes with HEXTORAW.
fn literal(cell: Option<String>, oracle_type: &OracleType) -> String {
    let Some(text) = cell else { return "NULL".into() };
    let quoted = || format!("'{}'", text.replace('\'', "''"));
    match oracle_type {
        _ if is_numeric_type_of(oracle_type) => text,
        OracleType::Date => format!("TO_DATE({}, 'YYYY-MM-DD HH24:MI:SS')", quoted()),
        OracleType::Timestamp(_) => format!("TO_TIMESTAMP({}, 'YYYY-MM-DD HH24:MI:SS.FF')", quoted()),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => format!("TO_TIMESTAMP_TZ({}, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM')", quoted()),
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => format!("HEXTORAW('{}')", text.trim_start_matches("\\x")),
        _ => quoted(),
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).to_string()
}

/// The first word of a statement, uppercase, past comments.
fn first_word(statement: &str) -> String {
    crate::sql_text::statement_verbs(statement).into_iter().next().unwrap_or_default()
}

/// Integer, decimal and floating point types: the grid right-aligns them.
pub(crate) fn is_numeric_type(column_type: &str) -> bool {
    let base = column_type.split('(').next().unwrap_or(column_type).trim().to_ascii_uppercase();
    matches!(base.as_str(), "NUMBER" | "FLOAT" | "BINARY_FLOAT" | "BINARY_DOUBLE" | "INTEGER")
}

fn base_type(column_type: &str) -> String {
    column_type.split('(').next().unwrap_or(column_type).trim().to_ascii_uppercase()
}

/// A grid cell, or with `limit` None the whole value: LOBs are cut on the
/// server; other values are turned into text by TablePort.
pub(crate) fn cell(column: &str, column_type: &str, limit: Option<u32>) -> String {
    match (base_type(column_type).as_str(), limit) {
        ("CLOB" | "NCLOB", Some(limit)) => format!("DBMS_LOB.SUBSTR({column}, {limit}, 1)"),
        // Each byte shows as two characters.
        ("BLOB", Some(limit)) => format!("DBMS_LOB.SUBSTR({column}, {}, 1)", limit / 2),
        ("XMLTYPE", _) => format!("XMLSERIALIZE(CONTENT {column} AS CLOB)"),
        _ => column.to_string(),
    }
}

/// Columns whose text LIKE can search.
pub(crate) fn is_searchable(column_type: &str) -> bool {
    !matches!(base_type(column_type).as_str(), "BLOB" | "RAW" | "LONG" | "LONG RAW" | "BFILE" | "XMLTYPE" | "SDO_GEOMETRY")
}

// MARK: - Scripts

/// The statements of a script, as SQL*Plus reads them: SQL ends at `;` or a
/// line holding only `/`; a PL/SQL block (BEGIN, DECLARE, CREATE
/// PROCEDURE…) keeps its own semicolons and ends at a `/` line. SQL loses
/// its `;`, which Oracle refuses; a block keeps its final END;.
pub(crate) fn split_script(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut block_comment = false;
    let mut line_start = true;
    let mut chars = sql.chars().peekable();
    let push = |statements: &mut Vec<String>, text: &str| {
        let text = text.trim();
        if !text.is_empty() {
            statements.push(text.to_string());
        }
    };
    while let Some(character) = chars.next() {
        // A line of only "/" ends any statement or block.
        if line_start && quote.is_none() && !block_comment && character == '/' {
            let rest: String = chars.clone().take_while(|next| *next != '\n').collect();
            if rest.trim().is_empty() {
                for _ in 0..rest.len() {
                    chars.next();
                }
                push(&mut statements, &current);
                current.clear();
                continue;
            }
        }
        line_start = character == '\n';
        if block_comment {
            current.push(character);
            if character == '*' && chars.peek() == Some(&'/') {
                current.push('/');
                chars.next();
                block_comment = false;
            }
            continue;
        }
        if let Some(open) = quote {
            current.push(character);
            if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                current.push(character);
            }
            '-' if chars.peek() == Some(&'-') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        current.push('\n');
                        line_start = true;
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                block_comment = true;
                current.push(character);
            }
            ';' if !is_plsql(&current) => {
                push(&mut statements, &current);
                current.clear();
            }
            _ => current.push(character),
        }
    }
    push(&mut statements, &current);
    statements
}

/// A PL/SQL block or a stored unit's definition, which holds semicolons.
fn is_plsql(statement: &str) -> bool {
    let words: Vec<String> = statement
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|word| !word.is_empty())
        .take(6)
        .map(str::to_ascii_uppercase)
        .collect();
    match words.first().map(String::as_str) {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => words
            .iter()
            .skip(1)
            .find(|word| !matches!(word.as_str(), "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE"))
            .is_some_and(|word| matches!(word.as_str(), "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE")),
        _ => false,
    }
}

// MARK: - Catalog

impl DatabaseSession {
    fn oracle_link(&self) -> Result<OracleLink, DatabaseError> {
        self.oracle().ok_or_else(|| DatabaseError::new("This needs an Oracle connection."))
    }

    /// The schemas of users Oracle doesn't maintain, with their tables,
    /// views and materialized views; the login's own schema always.
    pub(crate) async fn oracle_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let link = self.oracle_link()?;
        let user = quote_literal(&link.user)?;
        let rows = self
            .text_rows(format!(
                "SELECT u.username, o.name, o.kind FROM all_users u LEFT JOIN ( \
                   SELECT owner, table_name AS name, 'T' AS kind FROM all_tables \
                    WHERE nested = 'NO' AND secondary = 'N' AND dropped = 'NO' AND iot_type IS NULL \
                      AND (owner, table_name) NOT IN (SELECT owner, mview_name FROM all_mviews) \
                   UNION ALL SELECT owner, view_name, 'V' FROM all_views \
                   UNION ALL SELECT owner, mview_name, 'M' FROM all_mviews \
                 ) o ON o.owner = u.username \
                 WHERE u.oracle_maintained = 'N' OR u.username = {user} \
                 ORDER BY u.username, o.name"
            ))
            .await?;
        let mut schemas: Vec<SchemaContents> = Vec::new();
        for row in rows {
            let [Some(schema), name, kind] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else { continue };
            if schemas.last().is_none_or(|last| last.name != schema) {
                schemas.push(SchemaContents { name: schema.clone(), relations: Vec::new() });
            }
            let (Some(name), Some(kind), Some(contents)) = (name, kind, schemas.last_mut()) else { continue };
            let kind = match kind.as_str() {
                "V" => RelationKind::View,
                "M" => RelationKind::MaterializedView,
                _ => RelationKind::Table,
            };
            contents.relations.push(Relation { schema, name, kind });
        }
        // Users without objects only clutter the sidebar; the login's own stays.
        schemas.retain(|schema| !schema.relations.is_empty() || schema.name == link.user);
        Ok(schemas)
    }

    pub(crate) async fn oracle_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        Ok(self
            .oracle_column_rows(relation)
            .await?
            .into_iter()
            .map(|column| {
                let base = base_type(&column.data_type);
                let kind = match base.as_str() {
                    "DATE" => ColumnKind::Timestamp,
                    "TIMESTAMP" if column.data_type.contains("TIME ZONE") => ColumnKind::TimestampTz,
                    "TIMESTAMP" => ColumnKind::Timestamp,
                    "BOOLEAN" => ColumnKind::Boolean,
                    _ => ColumnKind::Other,
                };
                Column { is_numeric: is_numeric_type(&column.column_type), name: column.name, column_type: column.column_type, kind, enum_values: Vec::new() }
            })
            .collect())
    }

    async fn oracle_column_rows(&self, relation: &Relation) -> Result<Vec<OracleColumn>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT c.column_name, c.data_type, c.data_length, c.data_precision, c.data_scale, c.char_length, c.char_used, \
                        c.nullable, c.data_default, c.identity_column, c.virtual_column, cc.comments, \
                        (SELECT i.generation_type FROM all_tab_identity_cols i \
                          WHERE i.owner = c.owner AND i.table_name = c.table_name AND i.column_name = c.column_name) \
                 FROM all_tab_cols c LEFT JOIN all_col_comments cc \
                   ON cc.owner = c.owner AND cc.table_name = c.table_name AND cc.column_name = c.column_name \
                 WHERE c.owner = {} AND c.table_name = {} AND c.hidden_column = 'NO' ORDER BY c.column_id",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(data_type), length, precision, scale, char_length, char_used, nullable, default, identity, virtual_column, comment, generation] =
                    <[Option<String>; 13]>::try_from(row).ok()?
                else {
                    return None;
                };
                let column_type = type_text(&data_type, length.as_deref(), precision.as_deref(), scale.as_deref(), char_length.as_deref(), char_used.as_deref());
                let default = default.map(|text| text.trim().to_string()).filter(|text| !text.is_empty() && !text.eq_ignore_ascii_case("NULL"));
                let default_value = if identity.as_deref() == Some("YES") {
                    Some(format!("GENERATED {} AS IDENTITY", generation.unwrap_or_else(|| "BY DEFAULT".into())))
                } else if virtual_column.as_deref() == Some("YES") {
                    default.map(|expression| format!("GENERATED ALWAYS AS ({expression}) VIRTUAL"))
                } else {
                    default
                };
                Some(OracleColumn { name, data_type, column_type, nullable: nullable.as_deref() != Some("N"), default_value, comment })
            })
            .collect())
    }

    pub(crate) async fn oracle_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT cc.column_name FROM all_constraints c JOIN all_cons_columns cc \
                   ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 WHERE c.owner = {} AND c.table_name = {} AND c.constraint_type = 'P' ORDER BY cc.position",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Single-column foreign keys, which the grid follows with its arrows.
    pub(crate) async fn oracle_foreign_keys(&self, relation: &Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT cc.column_name, r.owner, r.table_name, rc.column_name FROM all_constraints c \
                 JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                 JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
                 JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
                 WHERE c.owner = {} AND c.table_name = {} AND c.constraint_type = 'R' \
                   AND (SELECT COUNT(*) FROM all_cons_columns x WHERE x.owner = c.owner AND x.constraint_name = c.constraint_name) = 1 \
                 ORDER BY c.constraint_name",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(column), Some(schema), Some(name), Some(referenced_column)] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                Some(ForeignKey { column, referenced_relation: Relation { schema, name, kind: RelationKind::Table }, referenced_column })
            })
            .collect())
    }

    /// The optimizer statistics' row count; None for views and tables never analyzed.
    pub(crate) async fn oracle_estimated_row_count(&self, relation: &Relation) -> Result<Option<u64>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT num_rows FROM all_tables WHERE owner = {} AND table_name = {}",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.first()).and_then(|cell| cell.as_deref()).and_then(|text| text.parse().ok()))
    }
}

struct OracleColumn {
    name: String,
    data_type: String,
    column_type: String,
    nullable: bool,
    default_value: Option<String>,
    comment: Option<String>,
}

/// The type as Oracle's tools write it: VARCHAR2(50 CHAR), NUMBER(10,2).
fn type_text(data_type: &str, length: Option<&str>, precision: Option<&str>, scale: Option<&str>, char_length: Option<&str>, char_used: Option<&str>) -> String {
    match data_type {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" => {
            let size = char_length.filter(|size| *size != "0").or(length).unwrap_or("1");
            if char_used == Some("C") && data_type.len() <= 8 && !data_type.starts_with('N') {
                format!("{data_type}({size} CHAR)")
            } else {
                format!("{data_type}({size})")
            }
        }
        "RAW" => format!("RAW({})", length.unwrap_or("1")),
        "NUMBER" => match (precision, scale) {
            (None, Some("0")) => "INTEGER".into(),
            (None, _) => "NUMBER".into(),
            (Some(precision), Some("0") | None) => format!("NUMBER({precision})"),
            (Some(precision), Some(scale)) => format!("NUMBER({precision},{scale})"),
        },
        "FLOAT" => format!("FLOAT({})", precision.unwrap_or("126")),
        other => other.to_string(),
    }
}

// MARK: - Structure

/// Whether an index to drop backs a primary key or unique constraint, which
/// is dropped with its constraint instead.
#[derive(Default)]
pub(crate) struct CatalogFacts {
    constraint_of_index: Option<String>,
}

impl DatabaseSession {
    pub(crate) async fn oracle_structure(&self, relation: &Relation) -> Result<TableStructure, DatabaseError> {
        let columns = self
            .oracle_column_rows(relation)
            .await?
            .into_iter()
            .map(|column| ColumnDetail {
                name: column.name,
                column_type: column.column_type,
                nullable: column.nullable,
                default_value: column.default_value,
                comment: column.comment,
            })
            .collect();
        let owner = quote_literal(&relation.schema)?;
        let table = quote_literal(&relation.name)?;
        let index_rows = self
            .text_rows(format!(
                "SELECT i.index_name, i.uniqueness, \
                        (SELECT c.constraint_type FROM all_constraints c WHERE c.owner = i.table_owner AND c.index_name = i.index_name \
                           AND c.constraint_type IN ('P', 'U') FETCH FIRST 1 ROWS ONLY), \
                        (SELECT LISTAGG('\"' || REPLACE(ic.column_name, '\"', '\"\"') || '\"' || CASE WHEN ic.descend = 'DESC' THEN ' DESC' END, ', ') \
                                WITHIN GROUP (ORDER BY ic.column_position) \
                         FROM all_ind_columns ic WHERE ic.index_owner = i.owner AND ic.index_name = i.index_name) \
                 FROM all_indexes i WHERE i.table_owner = {owner} AND i.table_name = {table} AND i.index_type <> 'LOB' \
                 ORDER BY i.index_name"
            ))
            .await?;
        let indexes = index_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(uniqueness), constraint, columns] = <[Option<String>; 4]>::try_from(row).ok()? else { return None };
                let is_primary = constraint.as_deref() == Some("P");
                let is_unique = uniqueness == "UNIQUE";
                let columns = columns.unwrap_or_default();
                let definition = if is_primary {
                    format!("PRIMARY KEY ({columns})")
                } else {
                    format!("CREATE {}INDEX {} ON {} ({columns})", if is_unique { "UNIQUE " } else { "" }, quote_identifier(&name), relation.quoted_name())
                };
                Some(IndexDetail { name, definition, is_unique, is_primary })
            })
            .collect();
        let constraint_rows = self
            .text_rows(format!(
                "SELECT c.constraint_name, c.constraint_type, \
                        (SELECT LISTAGG('\"' || REPLACE(cc.column_name, '\"', '\"\"') || '\"', ', ') WITHIN GROUP (ORDER BY cc.position) \
                         FROM all_cons_columns cc WHERE cc.owner = c.owner AND cc.constraint_name = c.constraint_name), \
                        (SELECT '\"' || r.owner || '\".\"' || r.table_name || '\" (' || \
                                LISTAGG('\"' || rc.column_name || '\"', ', ') WITHIN GROUP (ORDER BY rc.position) || ')' \
                         FROM all_constraints r JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name \
                         WHERE r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name GROUP BY r.owner, r.table_name), \
                        c.delete_rule, c.search_condition_vc \
                 FROM all_constraints c WHERE c.owner = {owner} AND c.table_name = {table} AND c.constraint_type IN ('P', 'U', 'R', 'C') \
                 ORDER BY DECODE(c.constraint_type, 'P', 1, 'R', 2, 'U', 3, 4), c.constraint_name"
            ))
            .await?;
        let constraints = constraint_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(kind), columns, referenced, delete_rule, check] = <[Option<String>; 6]>::try_from(row).ok()? else { return None };
                let columns = columns.unwrap_or_default();
                Some(match kind.as_str() {
                    "P" => ConstraintDetail { name, kind: ConstraintKind::PrimaryKey, definition: format!("PRIMARY KEY ({columns})") },
                    "U" => ConstraintDetail { name, kind: ConstraintKind::Unique, definition: format!("UNIQUE ({columns})") },
                    "R" => {
                        let action = delete_rule.filter(|rule| rule != "NO ACTION").map(|rule| format!(" ON DELETE {rule}")).unwrap_or_default();
                        ConstraintDetail {
                            name,
                            kind: ConstraintKind::ForeignKey,
                            definition: format!("FOREIGN KEY ({columns}) REFERENCES {}{action}", referenced.unwrap_or_default()),
                        }
                    }
                    _ => {
                        let check = check.unwrap_or_default();
                        // NOT NULL is kept as a check; the nullable column shows it already.
                        if check.trim_end().ends_with("IS NOT NULL") && !check.contains(" AND ") && !check.contains(" OR ") {
                            return None;
                        }
                        ConstraintDetail { name, kind: ConstraintKind::Check, definition: format!("CHECK ({check})") }
                    }
                })
            })
            .collect();
        Ok(TableStructure { columns, indexes, constraints })
    }

    pub(crate) async fn oracle_structure_change_sql(&self, relation: &Relation, change: StructureChange) -> Result<Vec<String>, DatabaseError> {
        let mut facts = CatalogFacts::default();
        if let StructureChange::DropIndex { name, .. } = &change {
            let rows = self
                .text_rows(format!(
                    "SELECT constraint_name FROM all_constraints WHERE owner = {} AND table_name = {} AND index_name = {} \
                       AND constraint_type IN ('P', 'U')",
                    quote_literal(&relation.schema)?,
                    quote_literal(&relation.name)?,
                    quote_literal(name)?
                ))
                .await?;
            facts.constraint_of_index = rows.into_iter().next().and_then(|row| row.into_iter().next().flatten());
        }
        structure_change_sql(relation, &change, &facts)
    }

    /// Runs a change's statements in turn: Oracle commits each DDL statement on its own.
    pub(crate) async fn oracle_apply_structure(&self, relation: Relation, change: StructureChange) -> Result<(), DatabaseError> {
        let statements = self.oracle_structure_change_sql(&relation, change).await?;
        let link = self.oracle_link()?;
        for statement in statements {
            link.execute(statement).await?;
        }
        Ok(())
    }
}

/// A column definition's tail for ADD: type, DEFAULT (before NOT NULL, as
/// Oracle requires), NULL.
fn column_tail(column: &crate::structure::ColumnSpec) -> Result<String, DatabaseError> {
    let column_type = column.column_type.trim();
    if column_type.is_empty() {
        return Err(DatabaseError::new("The column needs a type."));
    }
    let mut sql = column_type.to_string();
    match non_empty(&column.default_value) {
        Some(generated) if generated.starts_with("GENERATED ") => sql += &format!(" {generated}"),
        Some(default) => sql += &format!(" DEFAULT {default}"),
        None => {}
    }
    if !column.nullable {
        sql += " NOT NULL";
    }
    Ok(sql)
}

pub(crate) fn structure_change_sql(relation: &Relation, change: &StructureChange, facts: &CatalogFacts) -> Result<Vec<String>, DatabaseError> {
    let table = format!("ALTER TABLE {}", relation.quoted_name());
    let name = |name: &str| -> Result<String, DatabaseError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DatabaseError::new("The name is empty."));
        }
        Ok(quote_identifier(name))
    };
    let comment = |column: &str, text: Option<&str>| -> Result<String, DatabaseError> {
        Ok(format!("COMMENT ON COLUMN {}.{} IS {}", relation.quoted_name(), quote_identifier(column), quote_literal(text.unwrap_or_default())?))
    };
    Ok(match change {
        StructureChange::AddColumn { column } => {
            let mut statements = vec![format!("{table} ADD ({} {})", name(&column.name)?, column_tail(column)?)];
            if let Some(text) = non_empty(&column.comment) {
                statements.push(comment(column.name.trim(), Some(text))?);
            }
            statements
        }
        StructureChange::EditColumn { original, column } => {
            let new_name = column.name.trim();
            if new_name.is_empty() {
                return Err(DatabaseError::new("The column needs a name."));
            }
            let old_default = non_empty(&original.default_value);
            let new_default = non_empty(&column.default_value);
            if crate::structure::is_generated(&original.default_value) && old_default != new_default {
                return Err(DatabaseError::new("An identity or virtual column's definition is changed in SQL, not here."));
            }
            // MODIFY names only what changed: Oracle refuses NOT NULL on a column already NOT NULL.
            let mut parts = Vec::new();
            if column.column_type.trim() != original.column_type {
                parts.push(column.column_type.trim().to_string());
            }
            if old_default != new_default {
                parts.push(format!("DEFAULT {}", new_default.unwrap_or("NULL")));
            }
            if column.nullable != original.nullable {
                parts.push(if column.nullable { "NULL".into() } else { "NOT NULL".into() });
            }
            let mut statements = Vec::new();
            if !parts.is_empty() {
                statements.push(format!("{table} MODIFY ({} {})", quote_identifier(&original.name), parts.join(" ")));
            }
            if new_name != original.name {
                statements.push(format!("{table} RENAME COLUMN {} TO {}", quote_identifier(&original.name), name(new_name)?));
            }
            if non_empty(&column.comment) != non_empty(&original.comment) {
                statements.push(comment(new_name, non_empty(&column.comment))?);
            }
            statements
        }
        StructureChange::DropColumn { name: column, cascade } => {
            vec![format!("{table} DROP COLUMN {}{}", name(column)?, if *cascade { " CASCADE CONSTRAINTS" } else { "" })]
        }
        StructureChange::AddIndex { name: index, columns, unique } => {
            if columns.is_empty() {
                return Err(DatabaseError::new("Choose at least one column for the index."));
            }
            let columns = columns.iter().map(|column| quote_identifier(column)).collect::<Vec<_>>().join(", ");
            vec![format!(
                "CREATE {}INDEX {}.{} ON {} ({columns})",
                if *unique { "UNIQUE " } else { "" },
                quote_identifier(&relation.schema),
                name(index)?,
                relation.quoted_name()
            )]
        }
        StructureChange::DropIndex { name: index, cascade } => match &facts.constraint_of_index {
            Some(constraint) => vec![format!("{table} DROP CONSTRAINT {}{}", quote_identifier(constraint), if *cascade { " CASCADE" } else { "" })],
            None => vec![format!("DROP INDEX {}.{}", quote_identifier(&relation.schema), name(index)?)],
        },
        StructureChange::AddForeignKey { name: key, column, referenced, referenced_column, on_delete } => {
            let mut sql = format!("{table} ADD ");
            if let Some(key) = key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
                sql += &format!("CONSTRAINT {} ", quote_identifier(key));
            }
            sql += &format!("FOREIGN KEY ({}) REFERENCES {} ({})", name(column)?, referenced.quoted_name(), name(referenced_column)?);
            match on_delete.as_deref().map(|action| action.trim().to_uppercase()).filter(|action| !action.is_empty()).as_deref() {
                None | Some("NO ACTION") => {}
                Some(action @ ("CASCADE" | "SET NULL")) => sql += &format!(" ON DELETE {action}"),
                Some("RESTRICT") => return Err(DatabaseError::new("Oracle has no RESTRICT; the default, NO ACTION, refuses the delete in the same way.")),
                Some(other) => return Err(DatabaseError::new(format!("Oracle has no ON DELETE {other}."))),
            }
            vec![sql]
        }
        StructureChange::DropConstraint { name: constraint, cascade } => {
            vec![format!("{table} DROP CONSTRAINT {}{}", name(constraint)?, if *cascade { " CASCADE" } else { "" })]
        }
    })
}

/// Oracle's common built-in functions, for autocomplete.
pub(crate) const FUNCTIONS: &[&str] = &[
    "abs", "add_months", "avg", "cast", "ceil", "coalesce", "concat", "count", "current_date", "current_timestamp", "decode",
    "dense_rank", "extract", "floor", "greatest", "initcap", "instr", "json_object", "json_query", "json_value", "lag",
    "last_day", "lead", "least", "length", "listagg", "lower", "lpad", "ltrim", "max", "median", "min", "mod",
    "months_between", "nullif", "nvl", "nvl2", "rank", "regexp_count", "regexp_instr", "regexp_like", "regexp_replace",
    "regexp_substr", "replace", "round", "row_number", "rpad", "rtrim", "sign", "stddev", "substr", "sum", "sys_guid",
    "sysdate", "systimestamp", "to_char", "to_clob", "to_date", "to_number", "to_timestamp", "translate", "trim", "trunc",
    "upper", "variance",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_scripts_as_sqlplus() {
        let script = "SELECT 1 FROM dual;\nINSERT INTO t VALUES ('a;b');\nBEGIN\n  UPDATE t SET a = 1;\n  COMMIT;\nEND;\n/\n\
                      CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  NULL;\nEND;\n/\n-- a comment; not a statement\nSELECT 'x' FROM dual\n/\nSELECT 2 FROM dual";
        let statements = split_script(script);
        assert_eq!(
            statements,
            [
                "SELECT 1 FROM dual",
                "INSERT INTO t VALUES ('a;b')",
                "BEGIN\n  UPDATE t SET a = 1;\n  COMMIT;\nEND;",
                "CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  NULL;\nEND;",
                "SELECT 'x' FROM dual",
                "SELECT 2 FROM dual",
            ]
        );
        // A division is no statement end.
        assert_eq!(split_script("SELECT 4 / 2 FROM dual;"), ["SELECT 4 / 2 FROM dual"]);
    }

    #[test]
    fn writes_types_and_connect_strings() {
        assert_eq!(type_text("VARCHAR2", Some("200"), None, None, Some("50"), Some("C")), "VARCHAR2(50 CHAR)");
        assert_eq!(type_text("VARCHAR2", Some("50"), None, None, Some("50"), Some("B")), "VARCHAR2(50)");
        assert_eq!(type_text("NUMBER", Some("22"), Some("10"), Some("2"), None, None), "NUMBER(10,2)");
        assert_eq!(type_text("NUMBER", Some("22"), None, Some("0"), None, None), "INTEGER");
        assert_eq!(type_text("NUMBER", Some("22"), None, None, None, None), "NUMBER");
        let settings = ConnectionSettings {
            host: "db.example.com".into(),
            port: 1522,
            database: "app_high".into(),
            user: "admin".into(),
            password: None,
            tls: TlsMode::VerifyFull,
            ssh: None,
            read_only: false,
            engine: crate::Engine::Oracle,
        };
        assert_eq!(connect_string(&settings, None), "tcps://db.example.com:1522/app_high?ssl_server_dn_match=yes");
        let descriptor = ConnectionSettings { host: "(description=(address=(protocol=tcps)(port=1522)(host=x)))".into(), ..settings };
        assert!(connect_string(&descriptor, None).starts_with("(description="));
    }

    #[test]
    fn writes_structure_changes() {
        let relation = Relation { schema: "APP".into(), name: "USERS".into(), kind: RelationKind::Table };
        let original = ColumnDetail { name: "AGE".into(), column_type: "NUMBER".into(), nullable: false, default_value: Some("0".into()), comment: None };
        let column = crate::structure::ColumnSpec {
            name: "YEARS".into(),
            column_type: "NUMBER".into(),
            nullable: false,
            default_value: Some("1".into()),
            comment: Some("it's".into()),
        };
        let statements = structure_change_sql(&relation, &StructureChange::EditColumn { original, column }, &CatalogFacts::default()).unwrap();
        assert_eq!(
            statements,
            [
                "ALTER TABLE \"APP\".\"USERS\" MODIFY (\"AGE\" DEFAULT 1)",
                "ALTER TABLE \"APP\".\"USERS\" RENAME COLUMN \"AGE\" TO \"YEARS\"",
                "COMMENT ON COLUMN \"APP\".\"USERS\".\"YEARS\" IS 'it''s'",
            ]
        );
        let facts = CatalogFacts { constraint_of_index: Some("USERS_PK".into()) };
        let drop = structure_change_sql(&relation, &StructureChange::DropIndex { name: "USERS_PK".into(), cascade: false }, &facts).unwrap();
        assert_eq!(drop, ["ALTER TABLE \"APP\".\"USERS\" DROP CONSTRAINT \"USERS_PK\""]);
    }
}

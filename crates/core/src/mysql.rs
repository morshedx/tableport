//! MySQL and MariaDB. One session holds two connections: TablePort's own
//! SQL runs on one in ANSI_QUOTES and NO_BACKSLASH_ESCAPES mode, so the
//! shared code quotes names with `"` and values with `'` as for the other
//! engines, and a backslash in a value can't end its string early. The
//! user's SQL from the query editor runs on the other, in the server's own
//! mode, where `"` may quote a string as MySQL users expect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts, Value};
use tokio::sync::Mutex;

use crate::rows::{quote_identifier, quote_literal, Column, ColumnKind};
use crate::structure::{
    non_empty, ColumnDetail, ColumnSpec, ConstraintDetail, ConstraintKind, IndexDetail, StructureChange, TableStructure, REFERENTIAL_ACTIONS,
};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::{DatabaseError, DatabaseSession};

/// Schemas the server keeps for itself; hidden from the sidebar.
const SYSTEM_SCHEMAS: &str = "'mysql', 'information_schema', 'performance_schema', 'sys'";

#[derive(Clone)]
pub(crate) struct MysqlLink {
    internal: Arc<Mutex<Conn>>,
    editor: Arc<Mutex<Conn>>,
    /// The editor connection's id, for KILL QUERY (Cancel).
    editor_id: u32,
    /// For the separate connection that sends KILL QUERY.
    opts: Opts,
    /// "MySQL 8.4.13" or "MariaDB 11.4.13".
    pub(crate) version: String,
    pub(crate) is_mariadb: bool,
    closed: Arc<AtomicBool>,
    endpoint: String,
    /// Kept open as long as the connections.
    _tunnel: Option<Arc<Tunnel>>,
}

impl MysqlLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        let tunnel = match &settings.ssh {
            Some(ssh) => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            None => None,
        };
        let (host, port) = match &tunnel {
            Some(tunnel) => ("127.0.0.1".to_string(), tunnel.local_port),
            None => (settings.host.clone(), settings.port),
        };
        let database = Some(settings.database.trim()).filter(|name| !name.is_empty());
        let mut setup = Vec::new();
        if settings.read_only {
            // Every later transaction, and each statement on its own, refuses to write.
            setup.push("SET SESSION TRANSACTION READ ONLY".to_string());
        }
        let base = OptsBuilder::default()
            .ip_or_hostname(host)
            .tcp_port(port)
            .user(Some(settings.user.clone()))
            .pass(settings.password.clone())
            .db_name(database)
            .prefer_socket(false)
            // Probes an idle connection, so one that died without a goodbye is noticed.
            .tcp_keepalive(Some(Duration::from_secs(30)))
            // UPDATE reports the rows it matched, not only those it changed:
            // the edit check counts a row set to its own value as found.
            .client_found_rows(true)
            .connect_attribute("program_name", "tableport");
        let tls = |use_tls: bool| -> Option<SslOpts> {
            if !use_tls {
                return None;
            }
            let verify = settings.tls == TlsMode::VerifyFull;
            let mut ssl = SslOpts::default().with_danger_accept_invalid_certs(!verify).with_danger_skip_domain_validation(!verify);
            // Through SSH the connection goes to 127.0.0.1; the certificate is
            // still checked against the real server name.
            if verify && tunnel.is_some() {
                ssl = ssl.with_danger_tls_hostname_override(Some(settings.host.clone()));
            }
            Some(ssl)
        };
        let internal_setup = setup
            .iter()
            .cloned()
            .chain(std::iter::once(
                "SET SESSION sql_mode = CONCAT_WS(',', NULLIF(@@SESSION.sql_mode, ''), 'ANSI_QUOTES', 'NO_BACKSLASH_ESCAPES')".to_string(),
            ))
            .collect::<Vec<_>>();
        let connect = |use_tls: bool| {
            let internal = Opts::from(base.clone().ssl_opts(tls(use_tls)).setup(internal_setup.clone()));
            let editor = Opts::from(base.clone().ssl_opts(tls(use_tls)).setup(setup.clone()));
            let endpoint = endpoint.clone();
            async move {
                let both = async { tokio::try_join!(Conn::new(internal.clone()), Conn::new(editor)) };
                match tokio::time::timeout(Duration::from_secs(10), both).await {
                    Ok(Ok((internal_conn, editor_conn))) => Ok((internal_conn, editor_conn, internal)),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(mysql_async::Error::Other(format!("{endpoint} did not answer within 10 seconds.").into())),
                }
            }
        };
        let connected = match settings.tls {
            TlsMode::Disable => connect(false).await,
            TlsMode::Require | TlsMode::VerifyFull => connect(true).await,
            // TLS when the server has it, as libpq's "prefer".
            TlsMode::Prefer => match connect(true).await {
                Err(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)) => connect(false).await,
                other => other,
            },
        };
        let (mut internal, editor, opts) = connected.map_err(|error| DatabaseError::from_mysql(error, &endpoint))?;
        let version: String = internal
            .query_first("SELECT VERSION()")
            .await
            .map_err(|error| DatabaseError::from_mysql(error, &endpoint))?
            .unwrap_or_default();
        let is_mariadb = version.contains("MariaDB");
        let number = version.split('-').next().unwrap_or(&version).to_string();
        Ok(Self {
            editor_id: editor.id(),
            internal: Arc::new(Mutex::new(internal)),
            editor: Arc::new(Mutex::new(editor)),
            opts,
            version: format!("{} {number}", if is_mariadb { "MariaDB" } else { "MySQL" }),
            is_mariadb,
            closed: Arc::new(AtomicBool::new(false)),
            endpoint,
            _tunnel: tunnel,
        })
    }

    /// The error to show; a broken connection marks the link closed, so the
    /// app offers Reconnect.
    fn fail(&self, error: mysql_async::Error) -> DatabaseError {
        if error.is_fatal() {
            self.closed.store(true, Ordering::Relaxed);
        }
        DatabaseError::from_mysql(error, &self.endpoint)
    }

    async fn internal(&self) -> Result<tokio::sync::MutexGuard<'_, Conn>, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(self.internal.lock().await)
    }

    /// The first result's rows, every value as text (None for NULL).
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let mut conn = self.internal().await?;
        let mut result = conn.query_iter(sql).await.map_err(|error| self.fail(error))?;
        let rows: Vec<mysql_async::Row> = result.collect().await.map_err(|error| self.fail(error))?;
        result.drop_result().await.map_err(|error| self.fail(error))?;
        Ok(rows.into_iter().map(|row| row.unwrap().into_iter().map(text).collect()).collect())
    }

    /// Runs SQL and discards any rows; several statements may be separated by `;`.
    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        conn.query_drop(sql).await.map_err(|error| self.fail(error))
    }

    /// Runs `statements` in one transaction: all or none. Each of the first
    /// `checked` (updates and deletes by primary key) must find exactly one
    /// row; if one finds none, someone else changed or deleted it, and
    /// nothing is saved.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        conn.query_drop("START TRANSACTION").await.map_err(|error| self.fail(error))?;
        let mut outcome = Ok(());
        for (index, statement) in statements.iter().enumerate() {
            match conn.query_iter(statement.as_str()).await {
                Ok(result) => {
                    let changed = result.affected_rows();
                    if let Err(error) = result.drop_result().await {
                        outcome = Err(self.fail(error));
                        break;
                    }
                    if index < checked && changed == 0 {
                        outcome = Err(DatabaseError::new(
                            "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.",
                        ));
                        break;
                    }
                }
                Err(error) => {
                    outcome = Err(self.fail(error));
                    break;
                }
            }
        }
        let end = if outcome.is_ok() { "COMMIT" } else { "ROLLBACK" };
        let ended = conn.query_drop(end).await.map_err(|error| self.fail(error));
        outcome.and(ended)
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        match tokio::time::timeout(Duration::from_secs(5), conn.ping()).await {
            Ok(result) => result.map_err(|error| self.fail(error)),
            Err(_) => Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint))),
        }
    }

    /// Stops the editor's running statement from a separate connection.
    pub(crate) async fn cancel(&self) -> Result<(), DatabaseError> {
        let mut conn = Conn::new(self.opts.clone()).await.map_err(|error| DatabaseError::from_mysql(error, &self.endpoint))?;
        let result = conn.query_drop(format!("KILL QUERY {}", self.editor_id)).await;
        conn.disconnect().await.ok();
        result.map_err(|error| DatabaseError::from_mysql(error, &self.endpoint))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// The query editor's Run, on the editor connection in the server's own
    /// mode. The SQL goes to the server whole, which splits it into
    /// statements itself, so MySQL's own quoting rules apply. It runs as one
    /// transaction unless it manages its own; DDL commits on its own, as
    /// MySQL always does. Keeps the first `max_rows` rows of each result.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{cut, QueryResult, ResultSet};
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        let verbs = crate::sql_text::statement_verbs(&sql);
        let manages_transaction = verbs
            .iter()
            .any(|verb| matches!(verb.as_str(), "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "XA" | "SET"));
        let mut conn = self.editor.lock().await;
        let start = std::time::Instant::now();
        if !manages_transaction {
            conn.query_drop("START TRANSACTION").await.map_err(|error| self.fail(error))?;
        }
        let mut result_sets = Vec::new();
        let mut statement_counts = Vec::new();
        let outcome: Result<(), mysql_async::Error> = async {
            let mut result = conn.query_iter(sql).await?;
            while !result.is_empty() {
                let columns = result.columns().filter(|columns| !columns.is_empty());
                let Some(columns) = columns else {
                    statement_counts.push(result.affected_rows());
                    // Moves on to the next statement's result.
                    result.collect::<mysql_async::Row>().await?;
                    continue;
                };
                let binary: Vec<bool> = columns.iter().map(is_binary_column).collect();
                let mut kept = Vec::new();
                let mut total = 0u64;
                while let Some(row) = result.next().await? {
                    if total < max_rows {
                        let values = row
                            .unwrap()
                            .into_iter()
                            .zip(&binary)
                            .map(|(value, binary)| match value {
                                Value::Bytes(bytes) if *binary => Some(cut(&hex(&bytes))),
                                value => text(value).as_deref().map(cut),
                            })
                            .collect();
                        kept.push(values);
                    }
                    total += 1;
                }
                statement_counts.push(total);
                result_sets.push(ResultSet {
                    columns: columns.iter().map(|column| column.name_str().into_owned()).collect(),
                    rows: kept,
                    total_rows: total,
                    numeric_columns: columns.iter().map(|column| is_numeric_column(column.column_type())).collect(),
                });
            }
            // A later statement's error arrives only when its result is read.
            result.drop_result().await?;
            Ok(())
        }
        .await;
        if !manages_transaction {
            let end = if outcome.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let ended = conn.query_drop(end).await;
            if outcome.is_ok() {
                ended.map_err(|error| self.fail(error))?;
            }
        }
        outcome.map_err(|error| self.fail(error))?;
        Ok(QueryResult {
            result_sets,
            statement_counts,
            statement_verbs: verbs,
            elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }
}

impl MysqlLink {
    /// Writes the rows of `select` to `path` as they arrive, and returns how
    /// many it wrote. The SQL format is for MySQL's own tools: backtick
    /// names, and strings with backslashes escaped, as mysqldump writes them.
    pub(crate) async fn export(
        &self,
        select: String,
        columns: Vec<Column>,
        relation: Relation,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::ExportFormat;
        use tokio::io::AsyncWriteExt;
        let write_error = |error: std::io::Error| DatabaseError::new(format!("Can't write {path}: {error}."));
        let file = tokio::fs::File::create(&path).await.map_err(|error| DatabaseError::new(format!("Can't create {path}: {error}.")))?;
        let mut out = tokio::io::BufWriter::new(file);
        let mut conn = self.internal().await?;
        let mut result = conn.query_iter(select).await.map_err(|error| self.fail(error))?;
        let result_columns = result.columns().unwrap_or_else(|| Vec::new().into());
        let names: Vec<String> = result_columns.iter().map(|column| column.name_str().into_owned()).collect();
        let target = format!("{}.{}", backtick(&relation.schema), backtick(&relation.name));
        let quoted_names = if columns.is_empty() { names.clone() } else { columns.iter().map(|column| column.name.clone()).collect() }
            .iter()
            .map(|name| backtick(name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut count = 0u64;
        match format {
            ExportFormat::Csv => {
                let header = names.iter().map(|name| crate::sqlite::csv_field(name)).collect::<Vec<_>>().join(",");
                out.write_all(format!("{header}\n").as_bytes()).await.map_err(write_error)?;
            }
            ExportFormat::Json => out.write_all(b"[").await.map_err(write_error)?,
            ExportFormat::Sql => {}
        }
        while let Some(row) = result.next().await.map_err(|error| self.fail(error))? {
            let values = row.unwrap();
            let line = match format {
                ExportFormat::Csv => {
                    let fields = values
                        .into_iter()
                        .zip(result_columns.iter())
                        .map(|(value, column)| display(value, column).as_deref().map(crate::sqlite::csv_field).unwrap_or_default())
                        .collect::<Vec<_>>();
                    format!("{}\n", fields.join(","))
                }
                ExportFormat::Json => {
                    let object: serde_json::Map<String, serde_json::Value> =
                        names.iter().cloned().zip(values.into_iter().zip(result_columns.iter()).map(|(value, column)| json(value, column))).collect();
                    format!("{}{}", if count == 0 { "\n  " } else { ",\n  " }, serde_json::Value::Object(object))
                }
                ExportFormat::Sql => {
                    let literals =
                        values.into_iter().zip(result_columns.iter()).map(|(value, column)| literal(value, column)).collect::<Vec<_>>();
                    format!("INSERT INTO {target} ({quoted_names}) VALUES ({});\n", literals.join(", "))
                }
            };
            out.write_all(line.as_bytes()).await.map_err(write_error)?;
            count += 1;
        }
        result.drop_result().await.map_err(|error| self.fail(error))?;
        if format == ExportFormat::Json {
            out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).await.map_err(write_error)?;
        }
        out.flush().await.map_err(write_error)?;
        Ok(count)
    }

    /// One statement's rows as JSON objects, numbers as numbers, for AI access.
    pub(crate) async fn json_rows(&self, sql: String) -> Result<Vec<serde_json::Value>, DatabaseError> {
        let mut conn = self.internal().await?;
        let mut result = conn.query_iter(sql).await.map_err(|error| self.fail(error))?;
        let columns = result.columns().unwrap_or_else(|| Vec::new().into());
        let mut rows = Vec::new();
        while let Some(row) = result.next().await.map_err(|error| self.fail(error))? {
            let object: serde_json::Map<String, serde_json::Value> = columns
                .iter()
                .map(|column| column.name_str().into_owned())
                .zip(row.unwrap().into_iter().zip(columns.iter()).map(|(value, column)| json(value, column)))
                .collect();
            rows.push(serde_json::Value::Object(object));
        }
        result.drop_result().await.map_err(|error| self.fail(error))?;
        Ok(rows)
    }
}

/// `` `name` ``, with inner backticks doubled: MySQL's own quoting, for SQL
/// that leaves TablePort.
pub(crate) fn backtick(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A value as the grid shows it: bytes as `\x…` hex.
fn display(value: Value, column: &mysql_async::Column) -> Option<String> {
    match value {
        Value::Bytes(bytes) if is_binary_column(column) => Some(hex(&bytes)),
        value => text(value),
    }
}

fn json(value: Value, column: &mysql_async::Column) -> serde_json::Value {
    let Some(text) = display(value, column) else { return serde_json::Value::Null };
    if is_numeric_column(column.column_type()) {
        if let Ok(number) = text.parse::<serde_json::Number>() {
            return serde_json::Value::Number(number);
        }
    }
    if column.column_type() == mysql_async::consts::ColumnType::MYSQL_TYPE_JSON {
        if let Ok(parsed) = serde_json::from_str(&text) {
            return parsed;
        }
    }
    serde_json::Value::String(text)
}

/// A value as a MySQL literal in the server's default mode.
fn literal(value: Value, column: &mysql_async::Column) -> String {
    match value {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(bytes) if is_binary_column(column) => {
            format!("X'{}'", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
        }
        value => {
            let text = text(value).unwrap_or_default();
            if is_numeric_column(column.column_type()) && text.parse::<f64>().is_ok() {
                return text;
            }
            let mut quoted = String::with_capacity(text.len() + 2);
            quoted.push('\'');
            for character in text.chars() {
                match character {
                    '\\' => quoted.push_str("\\\\"),
                    '\'' => quoted.push_str("\\'"),
                    '\0' => quoted.push_str("\\0"),
                    '\n' => quoted.push_str("\\n"),
                    '\r' => quoted.push_str("\\r"),
                    '\u{1a}' => quoted.push_str("\\Z"),
                    other => quoted.push(other),
                }
            }
            quoted.push('\'');
            quoted
        }
    }
}

/// Bytes of BLOB, BINARY and VARBINARY results (the binary character set).
fn is_binary_column(column: &mysql_async::Column) -> bool {
    use mysql_async::consts::ColumnType::*;
    column.character_set() == 63
        && matches!(
            column.column_type(),
            MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB | MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR
        )
}

fn is_numeric_column(column_type: mysql_async::consts::ColumnType) -> bool {
    use mysql_async::consts::ColumnType::*;
    matches!(
        column_type,
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE
    )
}

/// `\x…`, as the grid shows bytes.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// A value of the text protocol as text: every value arrives as bytes.
pub(crate) fn text(value: Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())),
        Value::Int(number) => Some(number.to_string()),
        Value::UInt(number) => Some(number.to_string()),
        Value::Float(number) => Some(number.to_string()),
        Value::Double(number) => Some(number.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

/// Integer, decimal and floating point types: the grid right-aligns them.
pub(crate) fn is_numeric_type(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal" | "numeric" | "float" | "double" | "real"
    )
}

/// The labels of `enum('a','b')` or `set(…)`, with doubled quotes undone.
fn enum_labels(column_type: &str) -> Vec<String> {
    let Some(inner) = column_type.find('(').and_then(|open| column_type.rfind(')').map(|close| &column_type[open + 1..close])) else {
        return Vec::new();
    };
    let mut labels = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(next) = chars.next() {
        if next != '\'' {
            continue;
        }
        let mut label = String::new();
        while let Some(character) = chars.next() {
            if character == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    label.push('\'');
                } else {
                    break;
                }
            } else {
                label.push(character);
            }
        }
        labels.push(label);
    }
    labels
}

impl DatabaseSession {
    pub(crate) async fn mysql_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT s.SCHEMA_NAME, t.TABLE_NAME, t.TABLE_TYPE FROM information_schema.SCHEMATA s \
                 LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = s.SCHEMA_NAME AND t.TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
                 WHERE s.SCHEMA_NAME NOT IN ({SYSTEM_SCHEMAS}) ORDER BY s.SCHEMA_NAME, t.TABLE_NAME"
            ))
            .await?;
        let mut schemas: Vec<SchemaContents> = Vec::new();
        for row in rows {
            let [Some(schema), name, kind] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else {
                continue;
            };
            if schemas.last().is_none_or(|last| last.name != schema) {
                schemas.push(SchemaContents { name: schema.clone(), relations: Vec::new() });
            }
            let (Some(name), Some(kind), Some(contents)) = (name, kind, schemas.last_mut()) else { continue };
            let kind = if kind == "VIEW" { RelationKind::View } else { RelationKind::Table };
            contents.relations.push(Relation { schema, name, kind });
        }
        Ok(schemas)
    }

    pub(crate) async fn mysql_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(column_type), Some(data_type)] = <[Option<String>; 3]>::try_from(row).ok()? else {
                    return None;
                };
                let data_type = data_type.to_ascii_lowercase();
                let kind = match data_type.as_str() {
                    "date" => ColumnKind::Date,
                    "time" => ColumnKind::Time,
                    "datetime" | "timestamp" => ColumnKind::Timestamp,
                    "enum" => ColumnKind::Enum,
                    _ => ColumnKind::Other,
                };
                let enum_values = if kind == ColumnKind::Enum { enum_labels(&column_type) } else { Vec::new() };
                Some(Column { name, is_numeric: is_numeric_type(&data_type), column_type, kind, enum_values })
            })
            .collect())
    }

    pub(crate) async fn mysql_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Single-column foreign keys, which the grid follows with its arrows.
    pub(crate) async fn mysql_foreign_keys(&self, relation: &Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME \
                 FROM information_schema.KEY_COLUMN_USAGE k \
                 WHERE k.TABLE_SCHEMA = {schema} AND k.TABLE_NAME = {table} AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                   AND (SELECT count(*) FROM information_schema.KEY_COLUMN_USAGE other \
                        WHERE other.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND other.TABLE_NAME = k.TABLE_NAME \
                          AND other.CONSTRAINT_NAME = k.CONSTRAINT_NAME) = 1 \
                 ORDER BY k.CONSTRAINT_NAME",
                schema = quote_literal(&relation.schema)?,
                table = quote_literal(&relation.name)?
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

    /// The storage engine's estimate, kept in the catalog; None for views.
    pub(crate) async fn mysql_estimated_row_count(&self, relation: &Relation) -> Result<Option<u64>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND TABLE_TYPE = 'BASE TABLE'",
                quote_literal(&relation.schema)?,
                quote_literal(&relation.name)?
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.first()).and_then(|cell| cell.as_deref()).and_then(|text| text.parse().ok()))
    }
}

// MARK: - Structure

impl DatabaseSession {
    pub(crate) async fn mysql_structure(&self, relation: &Relation) -> Result<TableStructure, DatabaseError> {
        let is_mariadb = self.mysql().is_some_and(|link| link.is_mariadb);
        let schema = quote_literal(&relation.schema)?;
        let table = quote_literal(&relation.name)?;
        let column_rows = self
            .text_rows(format!(
                "SELECT c.COLUMN_NAME, c.COLUMN_TYPE, c.IS_NULLABLE, c.COLUMN_DEFAULT, c.EXTRA, c.COLUMN_COMMENT, \
                        c.GENERATION_EXPRESSION, c.DATA_TYPE, \
                        CASE WHEN c.COLLATION_NAME <> t.TABLE_COLLATION THEN c.COLLATION_NAME END \
                 FROM information_schema.COLUMNS c JOIN information_schema.TABLES t \
                   ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
                 WHERE c.TABLE_SCHEMA = {schema} AND c.TABLE_NAME = {table} ORDER BY c.ORDINAL_POSITION"
            ))
            .await?;
        let columns = column_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(column_type), Some(nullable), default, extra, comment, generation, Some(data_type), collation] =
                    <[Option<String>; 9]>::try_from(row).ok()?
                else {
                    return None;
                };
                // A collation other than the table's is part of the type, so
                // editing the column keeps it.
                let column_type = match collation {
                    Some(collation) => format!("{column_type} COLLATE {collation}"),
                    None => column_type,
                };
                Some(ColumnDetail {
                    name,
                    column_type,
                    nullable: nullable == "YES",
                    default_value: default_sql(
                        default,
                        extra.as_deref().unwrap_or_default(),
                        generation.as_deref().unwrap_or_default(),
                        &data_type,
                        is_mariadb,
                    ),
                    comment: comment.filter(|comment| !comment.is_empty()),
                })
            })
            .collect();

        let index_rows = self
            .text_rows(format!(
                "SELECT INDEX_NAME, NON_UNIQUE, GROUP_CONCAT(CONCAT('`', REPLACE(COLUMN_NAME, '`', '``'), '`') ORDER BY SEQ_IN_INDEX SEPARATOR ', ') \
                 FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {table} \
                 GROUP BY INDEX_NAME, NON_UNIQUE ORDER BY INDEX_NAME <> 'PRIMARY', INDEX_NAME"
            ))
            .await?;
        let indexes = index_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(non_unique), Some(columns)] = <[Option<String>; 3]>::try_from(row).ok()? else {
                    return None;
                };
                let is_primary = name == "PRIMARY";
                let is_unique = non_unique == "0";
                let definition = if is_primary {
                    format!("PRIMARY KEY ({columns})")
                } else {
                    format!("CREATE {}INDEX {} ON {} ({columns})", if is_unique { "UNIQUE " } else { "" }, backtick(&name), backtick(&relation.name))
                };
                Some(IndexDetail { name, definition, is_unique, is_primary })
            })
            .collect();

        let constraint_rows = self
            .text_rows(format!(
                "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, \
                        (SELECT GROUP_CONCAT(CONCAT('`', REPLACE(k.COLUMN_NAME, '`', '``'), '`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', ') \
                         FROM information_schema.KEY_COLUMN_USAGE k WHERE k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                           AND k.TABLE_NAME = tc.TABLE_NAME AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME), \
                        (SELECT CONCAT('`', MAX(k.REFERENCED_TABLE_SCHEMA), '`.`', MAX(k.REFERENCED_TABLE_NAME), '` (', \
                                GROUP_CONCAT(CONCAT('`', k.REFERENCED_COLUMN_NAME, '`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '), ')') \
                         FROM information_schema.KEY_COLUMN_USAGE k WHERE k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                           AND k.TABLE_NAME = tc.TABLE_NAME AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME), \
                        (SELECT r.DELETE_RULE FROM information_schema.REFERENTIAL_CONSTRAINTS r \
                         WHERE r.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND r.TABLE_NAME = tc.TABLE_NAME \
                           AND r.CONSTRAINT_NAME = tc.CONSTRAINT_NAME), \
                        (SELECT cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
                         WHERE cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME LIMIT 1) \
                 FROM information_schema.TABLE_CONSTRAINTS tc \
                 WHERE tc.TABLE_SCHEMA = {schema} AND tc.TABLE_NAME = {table} \
                 ORDER BY FIELD(tc.CONSTRAINT_TYPE, 'PRIMARY KEY', 'FOREIGN KEY', 'UNIQUE', 'CHECK'), tc.CONSTRAINT_NAME"
            ))
            .await?;
        let constraints = constraint_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(kind), columns, referenced, delete_rule, check] = <[Option<String>; 6]>::try_from(row).ok()? else {
                    return None;
                };
                let columns = columns.unwrap_or_default();
                let (kind, definition) = match kind.as_str() {
                    "PRIMARY KEY" => (ConstraintKind::PrimaryKey, format!("PRIMARY KEY ({columns})")),
                    "UNIQUE" => (ConstraintKind::Unique, format!("UNIQUE ({columns})")),
                    "FOREIGN KEY" => {
                        let action = delete_rule.filter(|rule| rule != "NO ACTION" && rule != "RESTRICT");
                        let action = action.map(|rule| format!(" ON DELETE {rule}")).unwrap_or_default();
                        (ConstraintKind::ForeignKey, format!("FOREIGN KEY ({columns}) REFERENCES {}{action}", referenced.unwrap_or_default()))
                    }
                    "CHECK" => (ConstraintKind::Check, format!("CHECK ({})", check.unwrap_or_default())),
                    _ => return None,
                };
                Some(ConstraintDetail { name, kind, definition })
            })
            .collect();
        Ok(TableStructure { columns, indexes, constraints })
    }

    /// The statements for a change, with the kind of a constraint to drop
    /// read from the catalog: MySQL drops each kind its own way.
    pub(crate) async fn mysql_structure_change_sql(&self, relation: &Relation, change: StructureChange) -> Result<Vec<String>, DatabaseError> {
        let mut kind = None;
        if let StructureChange::DropConstraint { name, .. } = &change {
            let rows = self
                .text_rows(format!(
                    "SELECT CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS \
                     WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND CONSTRAINT_NAME = {}",
                    quote_literal(&relation.schema)?,
                    quote_literal(&relation.name)?,
                    quote_literal(name)?
                ))
                .await?;
            kind = rows.into_iter().next().and_then(|row| row.into_iter().next().flatten()).map(|kind| match kind.as_str() {
                "PRIMARY KEY" => ConstraintKind::PrimaryKey,
                "FOREIGN KEY" => ConstraintKind::ForeignKey,
                "UNIQUE" => ConstraintKind::Unique,
                _ => ConstraintKind::Check,
            });
            if kind.is_none() {
                return Err(DatabaseError::new(format!("No constraint named {name}.")));
            }
        }
        structure_change_sql(relation, &change, kind)
    }
}

/// A column's default as SQL. MySQL 8 reports a literal default without its
/// quotes and an expression without its parentheses; MariaDB reports SQL.
/// AUTO_INCREMENT, ON UPDATE and generated columns are shown in its place,
/// as PostgreSQL shows identity columns.
fn default_sql(default: Option<String>, extra: &str, generation: &str, data_type: &str, is_mariadb: bool) -> Option<String> {
    let extra_lower = extra.to_ascii_lowercase();
    if !generation.is_empty() {
        let storage = if extra_lower.contains("stored") || extra_lower.contains("persistent") { "STORED" } else { "VIRTUAL" };
        return Some(format!("GENERATED ALWAYS AS ({generation}) {storage}"));
    }
    if extra_lower.contains("auto_increment") {
        return Some("AUTO_INCREMENT".into());
    }
    let base = match default {
        None => None,
        Some(text) if is_mariadb => Some(text).filter(|text| text != "NULL"),
        Some(text) if extra_lower.contains("default_generated") => {
            if text.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP") {
                Some(text)
            } else {
                Some(format!("({text})"))
            }
        }
        Some(text) if is_numeric_type(data_type) || data_type.eq_ignore_ascii_case("bit") => Some(text),
        Some(text) => quote_literal(&text).ok(),
    };
    let on_update = extra_lower.find("on update ").map(|start| extra[start + "on update ".len()..].trim().to_string());
    match (base, on_update) {
        (base, Some(on_update)) => Some(format!("{} ON UPDATE {on_update}", base.unwrap_or_else(|| "NULL".into()))),
        (base, None) => base,
    }
}

/// A column definition for ADD COLUMN and CHANGE COLUMN: MySQL restates the
/// whole column, so AUTO_INCREMENT and generated columns keep how they get
/// their values.
fn column_definition(column: &ColumnSpec) -> Result<String, DatabaseError> {
    let name = column.name.trim();
    if name.is_empty() {
        return Err(DatabaseError::new("The column needs a name."));
    }
    let column_type = column.column_type.trim();
    if column_type.is_empty() {
        return Err(DatabaseError::new("The column needs a type."));
    }
    let mut sql = format!("{} {column_type}", quote_identifier(name));
    let default = non_empty(&column.default_value);
    if let Some(generated) = default.filter(|text| text.starts_with("GENERATED ")) {
        sql += &format!(" {generated}");
    }
    sql += if column.nullable { " NULL" } else { " NOT NULL" };
    match default {
        Some("AUTO_INCREMENT") => sql += " AUTO_INCREMENT",
        Some(text) if text.starts_with("GENERATED ") => {}
        Some(text) => sql += &format!(" DEFAULT {text}"),
        None => {}
    }
    if let Some(comment) = non_empty(&column.comment) {
        sql += &format!(" COMMENT {}", quote_literal(comment)?);
    }
    Ok(sql)
}

/// The statements for a change, for TablePort's own connection (ANSI
/// quotes). `constraint_kind` says how to drop a constraint; without it,
/// MySQL's DROP CONSTRAINT is used.
pub(crate) fn structure_change_sql(
    relation: &Relation,
    change: &StructureChange,
    constraint_kind: Option<ConstraintKind>,
) -> Result<Vec<String>, DatabaseError> {
    let table = format!("ALTER TABLE {}", relation.quoted_name());
    let name = |name: &str| -> Result<String, DatabaseError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DatabaseError::new("The name is empty."));
        }
        Ok(quote_identifier(name))
    };
    Ok(match change {
        StructureChange::AddColumn { column } => vec![format!("{table} ADD COLUMN {};", column_definition(column)?)],
        StructureChange::EditColumn { original, column } => {
            let unchanged = column.name.trim() == original.name
                && column.column_type.trim() == original.column_type
                && column.nullable == original.nullable
                && non_empty(&column.default_value) == non_empty(&original.default_value)
                && non_empty(&column.comment) == non_empty(&original.comment);
            if unchanged {
                return Ok(Vec::new());
            }
            vec![format!("{table} CHANGE COLUMN {} {};", quote_identifier(&original.name), column_definition(column)?)]
        }
        StructureChange::DropColumn { name: column, .. } => vec![format!("{table} DROP COLUMN {};", name(column)?)],
        StructureChange::AddIndex { name: index, columns, unique } => {
            if columns.is_empty() {
                return Err(DatabaseError::new("Choose at least one column for the index."));
            }
            let columns = columns.iter().map(|column| quote_identifier(column)).collect::<Vec<_>>().join(", ");
            vec![format!("CREATE {}INDEX {} ON {} ({columns});", if *unique { "UNIQUE " } else { "" }, name(index)?, relation.quoted_name())]
        }
        StructureChange::DropIndex { name: index, .. } if index == "PRIMARY" => vec![format!("{table} DROP PRIMARY KEY;")],
        StructureChange::DropIndex { name: index, .. } => vec![format!("DROP INDEX {} ON {};", name(index)?, relation.quoted_name())],
        StructureChange::AddForeignKey { name: key, column, referenced, referenced_column, on_delete } => {
            let mut sql = format!("{table} ADD ");
            if let Some(key) = key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
                sql += &format!("CONSTRAINT {} ", quote_identifier(key));
            }
            sql += &format!("FOREIGN KEY ({}) REFERENCES {} ({})", name(column)?, referenced.quoted_name(), name(referenced_column)?);
            if let Some(action) = on_delete.as_deref().map(str::trim).filter(|action| !action.is_empty()) {
                let action = action.to_uppercase();
                if !REFERENTIAL_ACTIONS.contains(&action.as_str()) {
                    return Err(DatabaseError::new(format!("Unknown ON DELETE action: {action}")));
                }
                sql += &format!(" ON DELETE {action}");
            }
            vec![sql + ";"]
        }
        StructureChange::DropConstraint { name: constraint, .. } => vec![match constraint_kind {
            Some(ConstraintKind::PrimaryKey) => format!("{table} DROP PRIMARY KEY;"),
            Some(ConstraintKind::ForeignKey) => format!("{table} DROP FOREIGN KEY {};", name(constraint)?),
            Some(ConstraintKind::Unique) => format!("{table} DROP INDEX {};", name(constraint)?),
            _ => format!("{table} DROP CONSTRAINT {};", name(constraint)?),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_enum_labels() {
        assert_eq!(enum_labels("enum('small','it''s big','a,b')"), vec!["small", "it's big", "a,b"]);
        assert!(enum_labels("int").is_empty());
    }

    #[test]
    fn reads_defaults_of_both_servers() {
        assert_eq!(default_sql(Some("abc".into()), "", "", "varchar", false).as_deref(), Some("'abc'"));
        assert_eq!(default_sql(Some("0".into()), "", "", "int", false).as_deref(), Some("0"));
        assert_eq!(default_sql(Some("curdate()".into()), "DEFAULT_GENERATED", "", "date", false).as_deref(), Some("(curdate())"));
        assert_eq!(
            default_sql(Some("CURRENT_TIMESTAMP".into()), "DEFAULT_GENERATED on update CURRENT_TIMESTAMP", "", "timestamp", false).as_deref(),
            Some("CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP")
        );
        assert_eq!(default_sql(Some("'abc'".into()), "", "", "varchar", true).as_deref(), Some("'abc'"));
        assert_eq!(default_sql(Some("NULL".into()), "", "", "varchar", true), None);
        assert_eq!(default_sql(None, "auto_increment", "", "int", false).as_deref(), Some("AUTO_INCREMENT"));
        assert_eq!(default_sql(None, "STORED GENERATED", "(`a` + 1)", "int", false).as_deref(), Some("GENERATED ALWAYS AS ((`a` + 1)) STORED"));
    }
}

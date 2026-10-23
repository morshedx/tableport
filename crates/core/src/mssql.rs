//! SQL Server and Azure SQL, over TDS. One session holds two connections:
//! TablePort's own SQL runs on one, the query editor's on the other, so a
//! long query doesn't hold up the sidebar or the grid. Both start with
//! QUOTED_IDENTIFIER on, so the shared code quotes names with `"`; values
//! go as N'…' literals, so text outside the database's code page survives.
//! Values arrive typed and are turned into text here, so dates read the same
//! whatever the server's language.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use tiberius::{AuthMethod, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::rows::{Column, ColumnKind};
use crate::schema::{ForeignKey, Relation, RelationKind, SchemaContents};
use crate::session::{ConnectionSettings, TlsMode};
use crate::ssh::{open_tunnel, Tunnel};
use crate::structure::{
    non_empty, ColumnDetail, ColumnSpec, ConstraintDetail, ConstraintKind, IndexDetail, StructureChange, TableStructure,
};
use crate::{DatabaseError, DatabaseSession};

type Client = tiberius::Client<Compat<TcpStream>>;

/// Sent on both connections after login. A bare TDS login gets the server's
/// old defaults, which differ from what SSMS and the drivers set: TEXTSIZE
/// would cut long text at 4 KB, and DATEFORMAT follows the login's language,
/// which can read '2024-03-01' as January 3rd in a datetime.
const SETUP: &str = "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; \
                     SET CONCAT_NULL_YIELDS_NULL ON; SET ARITHABORT ON; SET NUMERIC_ROUNDABORT OFF; \
                     SET DATEFORMAT ymd; SET TEXTSIZE 2147483647; SET NOCOUNT OFF;";

const ROW_CHANGED: &str = "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.";

#[derive(Clone)]
pub(crate) struct MssqlLink {
    internal: Arc<Mutex<Client>>,
    editor: Arc<Mutex<Client>>,
    /// Cancel: wakes the editor's running query, which then sends an attention.
    stop: Arc<Notify>,
    /// "SQL Server 2022 (16.0.4135.4)" or "Azure SQL 12.0.2000.8".
    pub(crate) version: String,
    /// SQL Server has no read-only session; TablePort refuses writes itself.
    read_only: bool,
    closed: Arc<AtomicBool>,
    endpoint: String,
    /// Kept open as long as the connections.
    _tunnel: Option<Arc<Tunnel>>,
}

impl MssqlLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let endpoint = settings.endpoint();
        if settings.host.contains('\\') {
            return Err(DatabaseError::new(
                "Named instances (host\\instance) aren't supported yet. Use the instance's TCP port instead.",
            ));
        }
        let tunnel = match &settings.ssh {
            Some(ssh) => Some(Arc::new(open_tunnel(ssh, &settings.host, settings.port).await?)),
            None => None,
        };
        let mut config = Config::new();
        match &tunnel {
            Some(tunnel) => {
                config.host("127.0.0.1");
                config.port(tunnel.local_port);
            }
            None => {
                config.host(&settings.host);
                config.port(settings.port);
            }
        }
        if let Some(database) = Some(settings.database.trim()).filter(|name| !name.is_empty()) {
            config.database(database);
        }
        config.application_name("TablePort");
        config.authentication(AuthMethod::sql_server(&settings.user, settings.password.as_deref().unwrap_or_default()));
        config.encryption(match settings.tls {
            TlsMode::Disable => EncryptionLevel::NotSupported,
            // Encrypts when the server can.
            TlsMode::Prefer => EncryptionLevel::On,
            TlsMode::Require | TlsMode::VerifyFull => EncryptionLevel::Required,
        });
        if settings.tls == TlsMode::VerifyFull {
            // Through SSH the connection goes to 127.0.0.1; the certificate is
            // still checked against the real server name.
            if tunnel.is_some() {
                config.hostname_in_certificate(&settings.host);
            }
        } else {
            config.trust_cert();
        }
        let both = async { tokio::try_join!(connect(config.clone()), connect(config.clone())) };
        let (mut internal, editor) = match tokio::time::timeout(Duration::from_secs(10), both).await {
            Ok(result) => result.map_err(|error| DatabaseError::from_mssql(error, &endpoint))?,
            Err(_) => return Err(DatabaseError::new(format!("{endpoint} did not answer within 10 seconds."))),
        };
        let rows = internal
            .simple_query(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), CAST(SERVERPROPERTY('EngineEdition') AS int)",
            )
            .await
            .map_err(|error| DatabaseError::from_mssql(error, &endpoint))?
            .into_first_result()
            .await
            .map_err(|error| DatabaseError::from_mssql(error, &endpoint))?;
        let row: Vec<Option<String>> = rows.into_iter().next().map(row_text).unwrap_or_default();
        let number = row.first().cloned().flatten().unwrap_or_default();
        let edition = row.get(1).cloned().flatten().unwrap_or_default();
        Ok(Self {
            internal: Arc::new(Mutex::new(internal)),
            editor: Arc::new(Mutex::new(editor)),
            stop: Arc::new(Notify::new()),
            version: version_text(&number, &edition),
            read_only: settings.read_only,
            closed: Arc::new(AtomicBool::new(false)),
            endpoint,
            _tunnel: tunnel,
        })
    }

    /// The error to show; a broken connection marks the link closed, so the
    /// app offers Reconnect.
    fn fail(&self, error: tiberius::error::Error) -> DatabaseError {
        if matches!(error, tiberius::error::Error::Io { .. } | tiberius::error::Error::Protocol(_)) {
            self.closed.store(true, Ordering::Relaxed);
        }
        DatabaseError::from_mssql(error, &self.endpoint)
    }

    async fn internal(&self) -> Result<tokio::sync::MutexGuard<'_, Client>, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        Ok(self.internal.lock().await)
    }

    fn refuse_writes(&self) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        Ok(())
    }

    /// The first result's rows, every value as text (None for NULL).
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let mut conn = self.internal().await?;
        let rows = async { conn.simple_query(sql).await?.into_first_result().await }.await.map_err(|error| self.fail(error))?;
        Ok(rows.into_iter().map(row_text).collect())
    }

    /// Runs one batch and discards any rows.
    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut conn = self.internal().await?;
        async { conn.simple_query(sql).await?.into_results().await }.await.map(|_| ()).map_err(|error| self.fail(error))
    }

    /// Runs `statements` in one transaction: all or none. Each of the first
    /// `checked` (updates and deletes by primary key) must find exactly one
    /// row; if one finds none, someone else changed or deleted it, and
    /// nothing is saved.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut conn = self.internal().await?;
        run(&mut conn, "BEGIN TRANSACTION").await.map_err(|error| self.fail(error))?;
        let mut outcome = Ok(());
        for (index, statement) in statements.iter().enumerate() {
            // @@ROWCOUNT is the statement's own count, whatever its triggers did.
            let batch = if index < checked {
                format!("{statement};\nIF @@ROWCOUNT = 0 THROW 50000, N'{ROW_CHANGED}', 1;")
            } else {
                statement.clone()
            };
            if let Err(error) = run(&mut conn, &batch).await {
                outcome = Err(self.fail(error));
                break;
            }
        }
        // An error may have ended the transaction already.
        let end = if outcome.is_ok() { "COMMIT TRANSACTION" } else { "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION" };
        let ended = run(&mut conn, end).await.map_err(|error| self.fail(error));
        outcome.and(ended)
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        let mut conn = self.internal().await?;
        match tokio::time::timeout(Duration::from_secs(5), run(&mut conn, "SELECT 1")).await {
            Ok(result) => result.map_err(|error| self.fail(error)),
            Err(_) => {
                // The request was cut off midway; the connection can't be reused.
                self.closed.store(true, Ordering::Relaxed);
                Err(DatabaseError::new(format!("{} did not answer within 5 seconds.", self.endpoint)))
            }
        }
    }

    /// Stops the editor's running query.
    pub(crate) fn cancel(&self) {
        self.stop.notify_waiters();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// The query editor's Run. The SQL is split into batches at `GO` lines,
    /// as SSMS does, and runs as one transaction unless it manages its own.
    /// Keeps the first `max_rows` rows of each result.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::QueryResult;
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        let batches = split_batches(&sql);
        let words: Vec<String> = batches.iter().flat_map(|(batch, _)| words(batch)).map(|(word, _)| word).collect();
        let manages_transaction = manages_transaction(&words);
        if self.read_only {
            if manages_transaction {
                return Err(DatabaseError::new("This connection is read-only, so the SQL can't manage transactions."));
            }
            if let Some(verb) = batches
                .iter()
                .flat_map(|(batch, _)| crate::sql_text::statement_verbs(batch))
                .find(|verb| WRITE_VERBS.contains(&verb.as_str()))
            {
                return Err(DatabaseError::new(format!("This connection is read-only. {verb} was not run.")));
            }
        }
        let mut conn = self.editor.lock().await;
        let start = std::time::Instant::now();
        if !manages_transaction {
            run(&mut conn, "BEGIN TRANSACTION").await.map_err(|error| self.fail(error))?;
        }
        let stop = self.stop.notified();
        tokio::pin!(stop);
        stop.as_mut().enable();
        let outcome = tokio::select! {
            outcome = run_batches(&mut conn, &batches, max_rows) => outcome.map_err(|error| self.fail(error)),
            _ = &mut stop => Err(DatabaseError::new("The query was canceled.")),
        };
        if outcome.as_ref().is_err_and(|error| error.message() == "The query was canceled.") {
            conn.cancel_query().await.map_err(|error| self.fail(error))?;
        }
        if !manages_transaction {
            // A read-only connection never keeps what the SQL did.
            let end = if outcome.is_ok() && !self.read_only { "COMMIT TRANSACTION" } else { "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION" };
            let ended = run(&mut conn, end).await;
            if outcome.is_ok() {
                ended.map_err(|error| self.fail(error))?;
            }
        }
        let (result_sets, statement_counts, statement_verbs) = outcome?;
        Ok(QueryResult {
            result_sets,
            statement_counts,
            statement_verbs,
            elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    /// Writes the rows of `select` to `path` as they arrive, and returns how
    /// many it wrote. The SQL format is T-SQL: bracketed names, N'…' text and
    /// 0x… binary values.
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
        let mut stream = conn.simple_query(select).await.map_err(|error| self.fail(error))?;
        let names: Vec<String> = match stream.columns().await.map_err(|error| self.fail(error))? {
            Some(result_columns) => result_columns.iter().map(|column| column.name().to_string()).collect(),
            None => columns.iter().map(|column| column.name.clone()).collect(),
        };
        let target = format!("{}.{}", bracket(&relation.schema), bracket(&relation.name));
        let quoted_names = names.iter().map(|name| bracket(name)).collect::<Vec<_>>().join(", ");
        let mut count = 0u64;
        match format {
            ExportFormat::Csv => {
                let header = names.iter().map(|name| crate::sqlite::csv_field(name)).collect::<Vec<_>>().join(",");
                out.write_all(format!("{header}\n").as_bytes()).await.map_err(write_error)?;
            }
            ExportFormat::Json => out.write_all(b"[").await.map_err(write_error)?,
            ExportFormat::Sql => {}
        }
        while let Some(item) = stream.try_next().await.map_err(|error| self.fail(error))? {
            let QueryItem::Row(row) = item else { continue };
            let values: Vec<ColumnData<'static>> = row.into_iter().collect();
            let line = match format {
                ExportFormat::Csv => {
                    let fields = values.into_iter().map(|value| text(value).as_deref().map(crate::sqlite::csv_field).unwrap_or_default());
                    format!("{}\n", fields.collect::<Vec<_>>().join(","))
                }
                ExportFormat::Json => {
                    let object: serde_json::Map<String, serde_json::Value> = names.iter().cloned().zip(values.into_iter().map(json)).collect();
                    format!("{}{}", if count == 0 { "\n  " } else { ",\n  " }, serde_json::Value::Object(object))
                }
                ExportFormat::Sql => {
                    let literals = values.into_iter().map(literal).collect::<Vec<_>>();
                    format!("INSERT INTO {target} ({quoted_names}) VALUES ({});\n", literals.join(", "))
                }
            };
            out.write_all(line.as_bytes()).await.map_err(write_error)?;
            count += 1;
        }
        if format == ExportFormat::Json {
            out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).await.map_err(write_error)?;
        }
        out.flush().await.map_err(write_error)?;
        Ok(count)
    }

    /// One SELECT for AI access, on the editor connection. SQL Server has no
    /// read-only transaction, so the statement is checked first: one SELECT
    /// or WITH, without words that write, run code or reach other servers.
    /// It then runs in a transaction that is always rolled back, so a write
    /// that got past the check is undone. A timer cancels it after `timeout`;
    /// reading stops after `max_rows` rows.
    pub(crate) async fn read_only_json(&self, sql: &str, max_rows: u64, timeout: Duration) -> Result<Vec<serde_json::Value>, DatabaseError> {
        check_read_query(sql)?;
        let mut conn = self.editor.lock().await;
        run(&mut conn, "BEGIN TRANSACTION").await.map_err(|error| self.fail(error))?;
        let read = async {
            let mut stream = conn.simple_query(sql).await?;
            let mut names: Vec<String> = Vec::new();
            let mut rows = Vec::new();
            while let Some(item) = stream.try_next().await? {
                match item {
                    QueryItem::Metadata(metadata) if metadata.result_index() == 0 => {
                        names = metadata.columns().iter().map(|column| column.name().to_string()).collect();
                    }
                    QueryItem::Metadata(_) => break,
                    QueryItem::Row(row) => {
                        let object: serde_json::Map<String, serde_json::Value> = names.iter().cloned().zip(row.into_iter().map(json)).collect();
                        rows.push(serde_json::Value::Object(object));
                        if rows.len() as u64 > max_rows {
                            return Ok((rows, true));
                        }
                    }
                }
            }
            Ok::<_, tiberius::error::Error>((rows, false))
        };
        let outcome = tokio::time::timeout(timeout, read).await;
        let rows = match outcome {
            Ok(Ok((rows, stopped_early))) => {
                if stopped_early {
                    conn.cancel_query().await.map_err(|error| self.fail(error))?;
                }
                Ok(rows)
            }
            Ok(Err(error)) => Err(self.fail(error)),
            Err(_) => {
                conn.cancel_query().await.map_err(|error| self.fail(error))?;
                Err(DatabaseError::new(format!("The query ran longer than {} seconds and was stopped.", timeout.as_secs())))
            }
        };
        run(&mut conn, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await.map_err(|error| self.fail(error))?;
        rows
    }

    /// The estimated plan of one SELECT, as text: SHOWPLAN compiles the
    /// statement without running it.
    pub(crate) async fn explain(&self, sql: &str) -> Result<Vec<String>, DatabaseError> {
        check_read_query(sql)?;
        let mut conn = self.editor.lock().await;
        run(&mut conn, "SET SHOWPLAN_TEXT ON").await.map_err(|error| self.fail(error))?;
        let plan = async { conn.simple_query(sql).await?.into_results().await }.await;
        let off = run(&mut conn, "SET SHOWPLAN_TEXT OFF").await.map_err(|error| self.fail(error));
        let plan = plan.map_err(|error| self.fail(error))?;
        off?;
        // The first result repeats the statement; the plan follows.
        Ok(plan.into_iter().skip(1).flatten().filter_map(|row| row_text(row).into_iter().next().flatten()).collect())
    }
}

async fn connect(config: Config) -> Result<Client, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let mut client = match Client::connect(config.clone(), tcp.compat_write()).await {
        // Azure SQL may send the client on to the database's own node.
        Err(tiberius::error::Error::Routing { host, port }) => {
            let mut config = config;
            config.host(&host);
            config.port(port);
            let tcp = TcpStream::connect(config.get_addr()).await?;
            tcp.set_nodelay(true)?;
            Client::connect(config, tcp.compat_write()).await?
        }
        other => other?,
    };
    run(&mut client, SETUP).await?;
    Ok(client)
}

/// Runs a batch and reads its results to the end, so its errors arrive.
async fn run(conn: &mut Client, sql: &str) -> Result<(), tiberius::error::Error> {
    conn.simple_query(sql).await?.into_results().await.map(|_| ())
}

type Batches = (Vec<crate::query::ResultSet>, Vec<u64>, Vec<String>);

async fn run_batches(conn: &mut Client, batches: &[(String, u32)], max_rows: u64) -> Result<Batches, tiberius::error::Error> {
    use crate::query::{cut, ResultSet};
    let mut result_sets = Vec::new();
    let mut counts = Vec::new();
    let mut verbs = Vec::new();
    for (batch, repeat) in batches {
        for _ in 0..*repeat {
            // Only the execute call reports how many rows each statement
            // changed, and it drops rows; batches that return none use it.
            if returns_no_rows(batch) {
                let result = conn.execute(batch.as_str(), &[]).await?;
                let batch_verbs = crate::sql_text::statement_verbs(batch);
                let affected = result.rows_affected();
                let same_count = batch_verbs.len() == affected.len();
                for (index, count) in affected.iter().enumerate() {
                    counts.push(*count);
                    verbs.push(if same_count { batch_verbs[index].clone() } else { String::new() });
                }
                continue;
            }
            let mut stream = conn.simple_query(batch.as_str()).await?;
            while let Some(item) = stream.try_next().await? {
                match item {
                    QueryItem::Metadata(metadata) => {
                        result_sets.push(ResultSet {
                            columns: metadata.columns().iter().map(|column| column.name().to_string()).collect(),
                            rows: Vec::new(),
                            total_rows: 0,
                            numeric_columns: metadata.columns().iter().map(|column| is_numeric_column(column.column_type())).collect(),
                        });
                        counts.push(0);
                        verbs.push("SELECT".into());
                    }
                    QueryItem::Row(row) => {
                        let (Some(set), Some(count)) = (result_sets.last_mut(), counts.last_mut()) else { continue };
                        if set.total_rows < max_rows {
                            set.rows.push(row.into_iter().map(|value| text(value).as_deref().map(cut)).collect());
                        }
                        set.total_rows += 1;
                        *count += 1;
                    }
                }
            }
        }
    }
    Ok((result_sets, counts, verbs))
}

/// "SQL Server 2022 (16.0.4135.4)", or "Azure SQL 12.0.2000.8".
fn version_text(number: &str, edition: &str) -> String {
    // EngineEdition 5 is Azure SQL Database, 8 Azure SQL Managed Instance.
    if edition == "5" || edition == "8" {
        return format!("Azure SQL {number}");
    }
    let year = match number.split('.').next().unwrap_or_default() {
        "17" => "2025",
        "16" => "2022",
        "15" => "2019",
        "14" => "2017",
        "13" => "2016",
        "12" => "2014",
        "11" => "2012",
        _ => return format!("SQL Server {number}"),
    };
    format!("SQL Server {year} ({number})")
}

/// `[name]`, with inner `]` doubled: SQL Server's own quoting, for SQL that
/// people read.
pub(crate) fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `N'text'`, with inner quotes doubled.
pub(crate) fn literal_text(text: &str) -> Result<String, DatabaseError> {
    if text.contains('\0') {
        return Err(DatabaseError::new("The value contains a NUL character."));
    }
    Ok(format!("N'{}'", text.replace('\'', "''")))
}

/// `OBJECT_ID(N'[schema].[name]')`, for catalog queries.
fn object_id(relation: &Relation) -> Result<String, DatabaseError> {
    Ok(format!("OBJECT_ID({})", literal_text(&format!("{}.{}", bracket(&relation.schema), bracket(&relation.name)))?))
}

fn row_text(row: tiberius::Row) -> Vec<Option<String>> {
    row.into_iter().map(text).collect()
}

/// A value as the grid shows it: dates in ISO form, bytes as `\x…` hex,
/// bits as true or false.
pub(crate) fn text(value: ColumnData<'static>) -> Option<String> {
    use tiberius::time::chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    match value {
        ColumnData::U8(value) => value.map(|value| value.to_string()),
        ColumnData::I16(value) => value.map(|value| value.to_string()),
        ColumnData::I32(value) => value.map(|value| value.to_string()),
        ColumnData::I64(value) => value.map(|value| value.to_string()),
        ColumnData::F32(value) => value.map(|value| value.to_string()),
        ColumnData::F64(value) => value.map(|value| value.to_string()),
        ColumnData::Bit(value) => value.map(|value| value.to_string()),
        ColumnData::String(value) => value.map(|value| value.into_owned()),
        ColumnData::Guid(value) => value.map(|value| value.hyphenated().to_string().to_uppercase()),
        ColumnData::Binary(value) => value.map(|bytes| hex(&bytes)),
        ColumnData::Numeric(value) => value.map(|number| {
            // Numeric prints "5." for a scale of 0.
            let text = number.to_string();
            text.strip_suffix('.').map(str::to_string).unwrap_or(text)
        }),
        ColumnData::Xml(value) => value.map(|xml| xml.into_owned().into_string()),
        // chrono's Display is ISO with a space: 2024-03-01 09:30:00.5.
        ColumnData::DateTime(_) | ColumnData::DateTime2(_) | ColumnData::SmallDateTime(_) => {
            NaiveDateTime::from_sql(&value).ok().flatten().map(|time| time.to_string())
        }
        ColumnData::Date(_) => NaiveDate::from_sql(&value).ok().flatten().map(|date| date.to_string()),
        ColumnData::Time(_) => NaiveTime::from_sql(&value).ok().flatten().map(|time| time.to_string()),
        ColumnData::DateTimeOffset(_) => {
            DateTime::<FixedOffset>::from_sql(&value).ok().flatten().map(|time| time.to_string())
        }
    }
}

fn json(value: ColumnData<'static>) -> serde_json::Value {
    let is_number = matches!(
        value,
        ColumnData::U8(_) | ColumnData::I16(_) | ColumnData::I32(_) | ColumnData::I64(_) | ColumnData::F32(_) | ColumnData::F64(_) | ColumnData::Numeric(_)
    );
    let is_bit = matches!(value, ColumnData::Bit(_));
    let Some(text) = text(value) else { return serde_json::Value::Null };
    if is_number {
        if let Ok(number) = text.parse::<serde_json::Number>() {
            return serde_json::Value::Number(number);
        }
    }
    if is_bit {
        return serde_json::Value::Bool(text == "true");
    }
    serde_json::Value::String(text)
}

/// A value as a T-SQL literal.
fn literal(value: ColumnData<'static>) -> String {
    match value {
        ColumnData::Binary(Some(bytes)) => format!("0x{}", bytes.iter().map(|byte| format!("{byte:02X}")).collect::<String>()),
        ColumnData::Bit(Some(bit)) => if bit { "1" } else { "0" }.to_string(),
        value @ (ColumnData::U8(_)
        | ColumnData::I16(_)
        | ColumnData::I32(_)
        | ColumnData::I64(_)
        | ColumnData::F32(_)
        | ColumnData::F64(_)
        | ColumnData::Numeric(_)) => text(value).unwrap_or_else(|| "NULL".into()),
        value => match text(value) {
            Some(text) => format!("N'{}'", text.replace('\'', "''")),
            None => "NULL".into(),
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn is_numeric_column(column_type: ColumnType) -> bool {
    matches!(
        column_type,
        ColumnType::Int1
            | ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Intn
            | ColumnType::Float4
            | ColumnType::Float8
            | ColumnType::Floatn
            | ColumnType::Money
            | ColumnType::Money4
            | ColumnType::Decimaln
            | ColumnType::Numericn
    )
}

/// Integer, decimal, floating point and money types: the grid right-aligns them.
pub(crate) fn is_numeric_type(data_type: &str) -> bool {
    matches!(
        base_type(data_type).as_str(),
        "tinyint" | "smallint" | "int" | "bigint" | "decimal" | "numeric" | "float" | "real" | "money" | "smallmoney"
    )
}

/// "nvarchar(50)" → "nvarchar".
fn base_type(column_type: &str) -> String {
    column_type.split('(').next().unwrap_or(column_type).trim().to_ascii_lowercase()
}

/// A grid cell, or with `limit` None the whole value. Most values are read
/// as they are and turned into text here; long text and bytes are cut on
/// the server, and types the driver can't read are turned into text there.
pub(crate) fn cell(column: &str, column_type: &str, limit: Option<u32>) -> String {
    match base_type(column_type).as_str() {
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml" | "sysname" => match limit {
            Some(limit) => format!("LEFT(CAST({column} AS nvarchar(max)), {limit})"),
            None => format!("CAST({column} AS nvarchar(max))"),
        },
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => match limit {
            // Each byte shows as two characters.
            Some(limit) => format!("SUBSTRING(CAST({column} AS varbinary(max)), 1, {})", limit / 2),
            None => format!("CAST({column} AS varbinary(max))"),
        },
        "geography" | "geometry" | "hierarchyid" => match limit {
            Some(limit) => format!("LEFT({column}.ToString(), {limit})"),
            None => format!("{column}.ToString()"),
        },
        "sql_variant" => format!("CAST({column} AS nvarchar(4000))"),
        _ => column.to_string(),
    }
}

/// Columns whose text can be searched with LIKE: the CLR types and bytes
/// can't be cast to text.
pub(crate) fn is_searchable(column_type: &str) -> bool {
    !matches!(
        base_type(column_type).as_str(),
        "geography" | "geometry" | "hierarchyid" | "image" | "binary" | "varbinary" | "timestamp" | "rowversion"
    )
}

/// The type as SQL Server's tools write it: "nvarchar(50)", "decimal(10,2)",
/// "varchar(max)". Alias types keep their own name.
fn type_text(user_type: &str, system_type: &str, max_length: &str, precision: &str, scale: &str) -> String {
    if !user_type.eq_ignore_ascii_case(system_type) {
        return user_type.to_string();
    }
    let length = |divisor: i64| match max_length.parse::<i64>() {
        Ok(-1) => "max".to_string(),
        Ok(length) => (length / divisor).to_string(),
        Err(_) => max_length.to_string(),
    };
    match system_type {
        "varchar" | "char" | "varbinary" | "binary" => format!("{system_type}({})", length(1)),
        "nvarchar" | "nchar" => format!("{system_type}({})", length(2)),
        "decimal" | "numeric" => format!("{system_type}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" if scale != "7" => format!("{system_type}({scale})"),
        "float" if precision != "53" => format!("float({precision})"),
        _ => system_type.to_string(),
    }
}

/// A default as sys.default_constraints keeps it, "((0))" or "(getdate())",
/// without the parentheses around the whole.
fn default_text(definition: &str) -> String {
    let mut text = definition.trim();
    while let Some(inner) = text.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')) {
        // "(a)+(b)" starts and ends with parentheses that don't pair.
        let mut depth = 0i32;
        let mut quoted = false;
        let pairs = inner.chars().all(|character| {
            match character {
                '\'' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => depth -= 1,
                _ => {}
            }
            depth >= 0
        });
        if !pairs || depth != 0 {
            break;
        }
        text = inner.trim();
    }
    text.to_string()
}

// MARK: - T-SQL text

/// Words of T-SQL outside strings, quoted names and comments, uppercase,
/// with the depth of parentheses each one is at.
pub(crate) fn words(sql: &str) -> Vec<(String, u32)> {
    let mut words = Vec::new();
    let mut chars = sql.chars().peekable();
    let mut depth = 0u32;
    while let Some(character) = chars.next() {
        match character {
            '\'' | '"' => {
                // A doubled quote inside is one quote; reading on finds it again.
                for next in chars.by_ref() {
                    if next == character {
                        break;
                    }
                }
            }
            '[' => {
                while let Some(next) = chars.next() {
                    if next == ']' {
                        if chars.peek() == Some(&']') {
                            chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                // T-SQL comments nest.
                let mut nesting = 1;
                while let Some(next) = chars.next() {
                    match (next, chars.peek()) {
                        ('/', Some('*')) => {
                            chars.next();
                            nesting += 1;
                        }
                        ('*', Some('/')) => {
                            chars.next();
                            nesting -= 1;
                            if nesting == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            character if character.is_alphabetic() || character == '_' || character == '@' || character == '#' => {
                let mut word = String::from(character);
                while let Some(&next) = chars.peek() {
                    if next.is_alphanumeric() || matches!(next, '_' | '@' | '#' | '$') {
                        word.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                words.push((word.to_uppercase(), depth));
            }
            _ => {}
        }
    }
    words
}

/// The batches of a script, split at `GO` lines as SSMS and sqlcmd do, each
/// with how often to run it (`GO 3`). A GO inside a string or comment
/// doesn't split.
pub(crate) fn split_batches(sql: &str) -> Vec<(String, u32)> {
    #[derive(PartialEq, Clone, Copy)]
    enum State {
        Code,
        Quote(char),
        Bracket,
        Comment(u32),
    }
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut state = State::Code;
    for line in sql.split_inclusive('\n') {
        if state == State::Code {
            if let Some(repeat) = go_line(line) {
                if !current.trim().is_empty() {
                    batches.push((std::mem::take(&mut current), repeat));
                }
                current.clear();
                continue;
            }
        }
        let mut chars = line.chars().peekable();
        while let Some(character) = chars.next() {
            state = match (state, character) {
                (State::Code, '\'' | '"') => State::Quote(character),
                (State::Code, '[') => State::Bracket,
                (State::Code, '-') if chars.peek() == Some(&'-') => break,
                (State::Code, '/') if chars.peek() == Some(&'*') => {
                    chars.next();
                    State::Comment(1)
                }
                (State::Quote(quote), next) if next == quote => State::Code,
                (State::Bracket, ']') if chars.peek() == Some(&']') => {
                    chars.next();
                    State::Bracket
                }
                (State::Bracket, ']') => State::Code,
                (State::Comment(nesting), '/') if chars.peek() == Some(&'*') => {
                    chars.next();
                    State::Comment(nesting + 1)
                }
                (State::Comment(nesting), '*') if chars.peek() == Some(&'/') => {
                    chars.next();
                    if nesting == 1 { State::Code } else { State::Comment(nesting - 1) }
                }
                (state, _) => state,
            };
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        batches.push((current, 1));
    }
    batches
}

/// `GO` or `GO 3` alone on its line, maybe with a comment after it.
fn go_line(line: &str) -> Option<u32> {
    let line = line.split("--").next().unwrap_or(line).trim();
    let rest = line.get(..2).filter(|word| word.eq_ignore_ascii_case("GO")).map(|_| line[2..].trim())?;
    if rest.is_empty() {
        return Some(1);
    }
    rest.parse::<u32>().ok().filter(|count| (1..=1000).contains(count))
}

/// Statements SQL Server can't run in a transaction, and SQL that starts or
/// ends its own: the editor then runs the SQL as it is.
fn manages_transaction(words: &[String]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        let next = words.get(index + 1).map(String::as_str).unwrap_or_default();
        match word.as_str() {
            "COMMIT" | "ROLLBACK" | "BACKUP" | "RESTORE" | "RECONFIGURE" => true,
            "BEGIN" | "SAVE" => matches!(next, "TRAN" | "TRANSACTION" | "DISTRIBUTED"),
            "CREATE" | "ALTER" | "DROP" => matches!(next, "DATABASE" | "FULLTEXT"),
            _ => false,
        }
    })
}

const WRITE_VERBS: &[&str] =
    &["INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "CREATE", "ALTER", "DROP", "GRANT", "REVOKE", "DENY", "BULK", "DBCC", "KILL"];

/// A batch whose statements change things and return no rows, so the
/// editor can show how many rows each one changed.
fn returns_no_rows(batch: &str) -> bool {
    let words = words(batch);
    let Some((first, _)) = words.first() else { return false };
    // A procedure's or view's body is stored, not run.
    if matches!(first.as_str(), "CREATE" | "ALTER")
        && words.get(1).is_some_and(|(next, _)| matches!(next.as_str(), "PROCEDURE" | "PROC" | "FUNCTION" | "TRIGGER" | "VIEW"))
    {
        return true;
    }
    let verbs = crate::sql_text::statement_verbs(batch);
    if verbs.is_empty() || !verbs.iter().all(|verb| WRITE_VERBS.contains(&verb.as_str())) {
        return false;
    }
    // INSERT … SELECT reads into the table; any other SELECT, OUTPUT or
    // EXEC may return rows.
    let selects = words.iter().filter(|(word, depth)| word == "SELECT" && *depth == 0).count();
    let inserts = words.iter().filter(|(word, _)| word == "INSERT").count();
    selects <= inserts && !words.iter().any(|(word, _)| matches!(word.as_str(), "OUTPUT" | "EXEC" | "EXECUTE"))
}

/// Words that have no place in one read: they write, run code, manage
/// transactions or reach other servers.
const NOT_IN_A_READ: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "CREATE", "ALTER", "DROP", "GRANT", "REVOKE", "DENY", "EXEC", "EXECUTE",
    "INTO", "BEGIN", "COMMIT", "ROLLBACK", "SAVE", "DECLARE", "SET", "USE", "BACKUP", "RESTORE", "DBCC", "KILL", "SHUTDOWN",
    "RECONFIGURE", "WAITFOR", "BULK", "OPENROWSET", "OPENQUERY", "OPENDATASOURCE", "OPENXML", "GO", "CHECKPOINT", "RAISERROR",
    "THROW", "PRINT", "REVERT", "SETUSER", "READTEXT", "WRITETEXT", "UPDATETEXT",
];

/// One SELECT (or WITH … SELECT) and nothing that writes: AI access and
/// explain_query take only that. The transaction around it is rolled back
/// whatever passes here.
fn check_read_query(sql: &str) -> Result<(), DatabaseError> {
    let words = words(sql);
    match words.first().map(|(word, _)| word.as_str()) {
        Some("SELECT" | "WITH") => {}
        Some(other) => return Err(DatabaseError::new(format!("Only a SELECT or WITH query may run here, not {other}."))),
        None => return Err(DatabaseError::new("The query is empty.")),
    }
    if let Some((word, _)) = words.iter().find(|(word, _)| NOT_IN_A_READ.contains(&word.as_str())) {
        return Err(DatabaseError::new(format!("Only reads may run here; the query contains {word}.")));
    }
    if crate::sql_text::statements(sql).iter().filter(|statement| !statement.trim().is_empty()).count() > 1 {
        return Err(DatabaseError::new("Send one statement at a time."));
    }
    Ok(())
}

// MARK: - Catalog

/// Schemas SQL Server keeps for itself and its roles; hidden from the sidebar.
const SYSTEM_SCHEMAS: &str = "s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') AND s.name NOT LIKE 'db[_]%'";

impl DatabaseSession {
    pub(crate) async fn mssql_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows("SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name".into())
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    pub(crate) async fn mssql_schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT s.name, o.name, o.type FROM sys.schemas s \
                 LEFT JOIN sys.objects o ON o.schema_id = s.schema_id AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
                 WHERE {SYSTEM_SCHEMAS} ORDER BY s.name, o.name"
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
            let kind = if kind.trim() == "V" { RelationKind::View } else { RelationKind::Table };
            contents.relations.push(Relation { schema, name, kind });
        }
        Ok(schemas)
    }

    pub(crate) async fn mssql_columns(&self, relation: &Relation) -> Result<Vec<Column>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT c.name, TYPE_NAME(c.user_type_id), TYPE_NAME(c.system_type_id), c.max_length, c.precision, c.scale \
                 FROM sys.columns c WHERE c.object_id = {} ORDER BY c.column_id",
                object_id(relation)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(user_type), Some(system_type), Some(length), Some(precision), Some(scale)] =
                    <[Option<String>; 6]>::try_from(row).ok()?
                else {
                    return None;
                };
                let kind = match system_type.as_str() {
                    "date" => ColumnKind::Date,
                    "time" => ColumnKind::Time,
                    "datetime" | "datetime2" | "smalldatetime" => ColumnKind::Timestamp,
                    "datetimeoffset" => ColumnKind::TimestampTz,
                    "bit" => ColumnKind::Boolean,
                    _ => ColumnKind::Other,
                };
                let column_type = type_text(&user_type, &system_type, &length, &precision, &scale);
                Some(Column { name, is_numeric: is_numeric_type(&system_type), column_type, kind, enum_values: Vec::new() })
            })
            .collect())
    }

    pub(crate) async fn mssql_primary_key(&self, relation: &Relation) -> Result<Vec<String>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT c.name FROM sys.indexes i \
                 JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = {} AND i.is_primary_key = 1 ORDER BY ic.key_ordinal",
                object_id(relation)?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// Single-column foreign keys, which the grid follows with its arrows.
    pub(crate) async fn mssql_foreign_keys(&self, relation: &Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name FROM sys.foreign_keys fk \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 WHERE fk.parent_object_id = {} \
                   AND (SELECT COUNT(*) FROM sys.foreign_key_columns other WHERE other.constraint_object_id = fk.object_id) = 1 \
                 ORDER BY fk.name",
                object_id(relation)?
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

    /// The row count kept with the table's storage; None for views.
    pub(crate) async fn mssql_estimated_row_count(&self, relation: &Relation) -> Result<Option<u64>, DatabaseError> {
        let rows = self
            .text_rows(format!(
                "SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = {} AND p.index_id IN (0, 1)",
                object_id(relation)?
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.first()).and_then(|cell| cell.as_deref()).and_then(|text| text.parse().ok()))
    }
}

// MARK: - Structure

/// What a structure change needs to know that only the catalog has.
#[derive(Default)]
pub(crate) struct CatalogFacts {
    /// The name of the default constraint on the changed column.
    default_constraint: Option<String>,
    /// The index to drop is a primary key or unique constraint.
    index_is_constraint: bool,
}

impl DatabaseSession {
    pub(crate) async fn mssql_structure(&self, relation: &Relation) -> Result<TableStructure, DatabaseError> {
        let object = object_id(relation)?;
        let column_rows = self
            .text_rows(format!(
                "SELECT c.name, TYPE_NAME(c.user_type_id), TYPE_NAME(c.system_type_id), c.max_length, c.precision, c.scale, \
                        c.is_nullable, d.definition, \
                        CASE WHEN c.is_identity = 1 THEN CONCAT('IDENTITY(', CAST(ic.seed_value AS nvarchar(40)), ',', \
                             CAST(ic.increment_value AS nvarchar(40)), ')') END, \
                        cc.definition, cc.is_persisted, \
                        CASE WHEN c.collation_name <> CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS sysname) THEN c.collation_name END \
                 FROM sys.columns c \
                 LEFT JOIN sys.default_constraints d ON d.object_id = c.default_object_id \
                 LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
                 LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 WHERE c.object_id = {object} ORDER BY c.column_id"
            ))
            .await?;
        let columns = column_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(user_type), Some(system_type), Some(length), Some(precision), Some(scale), Some(nullable), default, identity, computed, persisted, collation] =
                    <[Option<String>; 12]>::try_from(row).ok()?
                else {
                    return None;
                };
                let mut column_type = type_text(&user_type, &system_type, &length, &precision, &scale);
                // A collation other than the database's is part of the type, so
                // editing the column keeps it.
                if let Some(collation) = collation {
                    column_type += &format!(" COLLATE {collation}");
                }
                let default_value = match (identity, computed) {
                    (Some(identity), _) => Some(identity),
                    (None, Some(computed)) => {
                        Some(format!("AS {computed}{}", if persisted.as_deref() == Some("true") { " PERSISTED" } else { "" }))
                    }
                    (None, None) => default.map(|default| default_text(&default)),
                };
                Some(ColumnDetail { name, column_type, nullable: nullable == "true", default_value, comment: None })
            })
            .collect();

        let index_rows = self
            .text_rows(format!(
                "SELECT i.name, i.is_unique, i.is_primary_key, i.type_desc, \
                        (SELECT STRING_AGG(CAST(QUOTENAME(c.name) + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END AS nvarchar(max)), ', ') \
                                WITHIN GROUP (ORDER BY ic.key_ordinal) \
                         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0), \
                        (SELECT STRING_AGG(CAST(QUOTENAME(c.name) AS nvarchar(max)), ', ') WITHIN GROUP (ORDER BY ic.index_column_id) \
                         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 1), \
                        i.filter_definition \
                 FROM sys.indexes i WHERE i.object_id = {object} AND i.type > 0 AND i.is_hypothetical = 0 \
                 ORDER BY i.is_primary_key DESC, i.name"
            ))
            .await?;
        let table = format!("{}.{}", bracket(&relation.schema), bracket(&relation.name));
        let indexes = index_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(unique), Some(primary), Some(kind), columns, included, filter] = <[Option<String>; 7]>::try_from(row).ok()?
                else {
                    return None;
                };
                let is_primary = primary == "true";
                let is_unique = unique == "true";
                let columns = columns.unwrap_or_default();
                let kind = kind.replace('_', " ");
                let mut definition = if is_primary {
                    format!("PRIMARY KEY {kind} ({columns})")
                } else {
                    format!("CREATE {}{kind} INDEX {} ON {table} ({columns})", if is_unique { "UNIQUE " } else { "" }, bracket(&name))
                };
                if let Some(included) = included {
                    definition += &format!(" INCLUDE ({included})");
                }
                if let Some(filter) = filter {
                    definition += &format!(" WHERE {filter}");
                }
                Some(IndexDetail { name, definition, is_unique, is_primary })
            })
            .collect();

        let mut constraints = Vec::new();
        let key_rows = self
            .text_rows(format!(
                "SELECT k.name, k.type, \
                        (SELECT STRING_AGG(CAST(QUOTENAME(c.name) AS nvarchar(max)), ', ') WITHIN GROUP (ORDER BY ic.key_ordinal) \
                         FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                         WHERE ic.object_id = k.parent_object_id AND ic.index_id = k.unique_index_id AND ic.is_included_column = 0) \
                 FROM sys.key_constraints k WHERE k.parent_object_id = {object} ORDER BY k.type, k.name"
            ))
            .await?;
        for row in key_rows {
            let Ok([Some(name), Some(kind), columns]) = <[Option<String>; 3]>::try_from(row) else { continue };
            let columns = columns.unwrap_or_default();
            constraints.push(if kind.trim() == "PK" {
                ConstraintDetail { name, kind: ConstraintKind::PrimaryKey, definition: format!("PRIMARY KEY ({columns})") }
            } else {
                ConstraintDetail { name, kind: ConstraintKind::Unique, definition: format!("UNIQUE ({columns})") }
            });
        }
        let foreign_rows = self
            .text_rows(format!(
                "SELECT fk.name, \
                        (SELECT STRING_AGG(CAST(QUOTENAME(c.name) AS nvarchar(max)), ', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) \
                         FROM sys.foreign_key_columns fkc JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id \
                         WHERE fkc.constraint_object_id = fk.object_id), \
                        QUOTENAME(SCHEMA_NAME(rt.schema_id)) + '.' + QUOTENAME(rt.name), \
                        (SELECT STRING_AGG(CAST(QUOTENAME(c.name) AS nvarchar(max)), ', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) \
                         FROM sys.foreign_key_columns fkc JOIN sys.columns c ON c.object_id = fkc.referenced_object_id AND c.column_id = fkc.referenced_column_id \
                         WHERE fkc.constraint_object_id = fk.object_id), \
                        fk.delete_referential_action_desc, fk.update_referential_action_desc \
                 FROM sys.foreign_keys fk JOIN sys.objects rt ON rt.object_id = fk.referenced_object_id \
                 WHERE fk.parent_object_id = {object} ORDER BY fk.name"
            ))
            .await?;
        for row in foreign_rows {
            let Ok([Some(name), columns, Some(referenced), referenced_columns, on_delete, on_update]) = <[Option<String>; 6]>::try_from(row) else {
                continue;
            };
            let mut definition =
                format!("FOREIGN KEY ({}) REFERENCES {referenced} ({})", columns.unwrap_or_default(), referenced_columns.unwrap_or_default());
            for (event, action) in [("DELETE", on_delete), ("UPDATE", on_update)] {
                if let Some(action) = action.filter(|action| action != "NO_ACTION") {
                    definition += &format!(" ON {event} {}", action.replace('_', " "));
                }
            }
            constraints.push(ConstraintDetail { name, kind: ConstraintKind::ForeignKey, definition });
        }
        let check_rows = self
            .text_rows(format!("SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = {object} ORDER BY name"))
            .await?;
        for row in check_rows {
            let Ok([Some(name), Some(definition)]) = <[Option<String>; 2]>::try_from(row) else { continue };
            constraints.push(ConstraintDetail { name, kind: ConstraintKind::Check, definition: format!("CHECK {definition}") });
        }
        Ok(TableStructure { columns, indexes, constraints })
    }

    /// The statements for a change, with what the catalog knows: the name of
    /// a column's default constraint, and whether an index belongs to a
    /// constraint.
    pub(crate) async fn mssql_structure_change_sql(&self, relation: &Relation, change: StructureChange) -> Result<Vec<String>, DatabaseError> {
        let mut facts = CatalogFacts::default();
        let column = match &change {
            StructureChange::EditColumn { original, .. } => Some(original.name.clone()),
            StructureChange::DropColumn { name, .. } => Some(name.clone()),
            _ => None,
        };
        if let Some(column) = column {
            let rows = self
                .text_rows(format!(
                    "SELECT d.name FROM sys.default_constraints d JOIN sys.columns c \
                       ON c.object_id = d.parent_object_id AND c.column_id = d.parent_column_id \
                     WHERE d.parent_object_id = {} AND c.name = {}",
                    object_id(relation)?,
                    literal_text(&column)?
                ))
                .await?;
            facts.default_constraint = rows.into_iter().next().and_then(|row| row.into_iter().next().flatten());
        }
        if let StructureChange::DropIndex { name, .. } = &change {
            let rows = self
                .text_rows(format!(
                    "SELECT CASE WHEN is_primary_key = 1 OR is_unique_constraint = 1 THEN 1 ELSE 0 END FROM sys.indexes \
                     WHERE object_id = {} AND name = {}",
                    object_id(relation)?,
                    literal_text(name)?
                ))
                .await?;
            facts.index_is_constraint = rows.into_iter().next().and_then(|row| row.into_iter().next().flatten()).as_deref() == Some("1");
        }
        structure_change_sql(relation, &change, &facts)
    }

    /// Runs a change's statements as one transaction: SQL Server's DDL is
    /// transactional, so a failed step leaves the table as it was.
    pub(crate) async fn mssql_apply_structure(&self, relation: Relation, change: StructureChange) -> Result<(), DatabaseError> {
        let statements = self.mssql_structure_change_sql(&relation, change).await?;
        let Some(link) = self.mssql() else { return Err(DatabaseError::new("This needs a SQL Server connection.")) };
        crate::on_runtime(async move { link.apply_checked(statements, 0).await }).await
    }
}

/// The parts of a column definition after its name: type, NULL, and how it
/// gets its value. IDENTITY(…) and AS (…) are written where a default would be.
fn column_tail(column: &ColumnSpec) -> Result<String, DatabaseError> {
    let column_type = column.column_type.trim();
    let default = non_empty(&column.default_value);
    if let Some(computed) = default.filter(|text| text.starts_with("AS ")) {
        return Ok(computed.to_string());
    }
    if column_type.is_empty() {
        return Err(DatabaseError::new("The column needs a type."));
    }
    let mut sql = format!("{column_type}{}", if column.nullable { " NULL" } else { " NOT NULL" });
    match default {
        Some(identity) if identity.to_ascii_uppercase().starts_with("IDENTITY") => sql += &format!(" {identity}"),
        Some(default) => sql += &format!(" DEFAULT {default}"),
        None => {}
    }
    Ok(sql)
}

fn is_value_rule(default: Option<&str>) -> bool {
    default.is_some_and(|text| text.to_ascii_uppercase().starts_with("IDENTITY") || text.starts_with("AS "))
}

/// The statements for a change. `facts` come from the catalog; without
/// them a column's default can't be dropped.
pub(crate) fn structure_change_sql(relation: &Relation, change: &StructureChange, facts: &CatalogFacts) -> Result<Vec<String>, DatabaseError> {
    let table_name = format!("{}.{}", bracket(&relation.schema), bracket(&relation.name));
    let table = format!("ALTER TABLE {table_name}");
    let name = |name: &str| -> Result<String, DatabaseError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DatabaseError::new("The name is empty."));
        }
        Ok(bracket(name))
    };
    let drop_default = || facts.default_constraint.as_deref().map(|constraint| format!("{table} DROP CONSTRAINT {};", bracket(constraint)));
    Ok(match change {
        StructureChange::AddColumn { column } => vec![format!("{table} ADD {} {};", name(&column.name)?, column_tail(column)?)],
        StructureChange::EditColumn { original, column } => {
            let new_name = column.name.trim();
            if new_name.is_empty() {
                return Err(DatabaseError::new("The column needs a name."));
            }
            let old_default = non_empty(&original.default_value);
            let new_default = non_empty(&column.default_value);
            let retyped = column.column_type.trim() != original.column_type || column.nullable != original.nullable;
            if (is_value_rule(old_default) || is_value_rule(new_default)) && (old_default != new_default || retyped) {
                return Err(DatabaseError::new(
                    "SQL Server can't change an identity or computed column in place. Add a new column and copy the values instead.",
                ));
            }
            let mut statements = Vec::new();
            let column_name = bracket(&original.name);
            // ALTER COLUMN refuses a column that a default constraint depends
            // on, so the default comes off first and goes back on after.
            let redo_default = old_default.is_some() && (old_default != new_default || retyped);
            if redo_default {
                statements.push(drop_default().ok_or_else(|| DatabaseError::new("The column's default constraint wasn't found."))?);
            }
            if retyped {
                statements.push(format!(
                    "{table} ALTER COLUMN {column_name} {}{};",
                    column.column_type.trim(),
                    if column.nullable { " NULL" } else { " NOT NULL" }
                ));
            }
            if let Some(default) = new_default.filter(|_| redo_default || old_default != new_default) {
                statements.push(format!("{table} ADD DEFAULT {default} FOR {column_name};"));
            }
            if new_name != original.name {
                statements.push(format!(
                    "EXEC sp_rename {}, {}, 'COLUMN';",
                    literal_text(&format!("{table_name}.{column_name}"))?,
                    literal_text(new_name)?
                ));
            }
            statements
        }
        StructureChange::DropColumn { name: column, .. } => {
            // A column with a default constraint can't be dropped before it.
            let mut statements: Vec<String> = drop_default().into_iter().collect();
            statements.push(format!("{table} DROP COLUMN {};", name(column)?));
            statements
        }
        StructureChange::AddIndex { name: index, columns, unique } => {
            if columns.is_empty() {
                return Err(DatabaseError::new("Choose at least one column for the index."));
            }
            let columns = columns.iter().map(|column| bracket(column)).collect::<Vec<_>>().join(", ");
            vec![format!("CREATE {}INDEX {} ON {table_name} ({columns});", if *unique { "UNIQUE " } else { "" }, name(index)?)]
        }
        StructureChange::DropIndex { name: index, .. } if facts.index_is_constraint => vec![format!("{table} DROP CONSTRAINT {};", name(index)?)],
        StructureChange::DropIndex { name: index, .. } => vec![format!("DROP INDEX {} ON {table_name};", name(index)?)],
        StructureChange::AddForeignKey { name: key, column, referenced, referenced_column, on_delete } => {
            let mut sql = format!("{table} ADD ");
            if let Some(key) = key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
                sql += &format!("CONSTRAINT {} ", bracket(key));
            }
            sql += &format!(
                "FOREIGN KEY ({}) REFERENCES {}.{} ({})",
                name(column)?,
                bracket(&referenced.schema),
                bracket(&referenced.name),
                name(referenced_column)?
            );
            if let Some(action) = on_delete.as_deref().map(str::trim).filter(|action| !action.is_empty()) {
                let action = action.to_uppercase();
                if action == "RESTRICT" {
                    return Err(DatabaseError::new("SQL Server has no RESTRICT; NO ACTION refuses the delete in the same way."));
                }
                if !["NO ACTION", "CASCADE", "SET NULL", "SET DEFAULT"].contains(&action.as_str()) {
                    return Err(DatabaseError::new(format!("Unknown ON DELETE action: {action}")));
                }
                sql += &format!(" ON DELETE {action}");
            }
            vec![sql + ";"]
        }
        StructureChange::DropConstraint { name: constraint, .. } => vec![format!("{table} DROP CONSTRAINT {};", name(constraint)?)],
    })
}

/// SQL Server's common built-in functions, for autocomplete.
pub(crate) const FUNCTIONS: &[&str] = &[
    "abs", "acos", "ascii", "asin", "atan", "atn2", "avg", "cast", "ceiling", "char", "charindex", "checksum", "choose",
    "coalesce", "concat", "concat_ws", "convert", "cos", "count", "count_big", "current_timestamp", "current_user",
    "datalength", "dateadd", "datediff", "datediff_big", "datefromparts", "datename", "datepart", "datetrunc", "day",
    "db_id", "db_name", "degrees", "dense_rank", "difference", "eomonth", "exp", "first_value", "floor", "format",
    "getdate", "getutcdate", "greatest", "grouping", "hashbytes", "iif", "isdate", "isjson", "isnull", "isnumeric",
    "json_modify", "json_query", "json_value", "lag", "last_value", "lead", "least", "left", "len", "log", "log10", "lower",
    "ltrim", "max", "min", "month", "newid", "newsequentialid", "nchar", "ntile", "nullif", "object_id", "object_name",
    "parse", "patindex", "pi", "power", "quotename", "radians", "rand", "rank", "replace", "replicate", "reverse", "right",
    "round", "row_number", "rtrim", "schema_name", "scope_identity", "sign", "sin", "soundex", "space", "sqrt", "square",
    "stdev", "str", "string_agg", "string_split", "stuff", "substring", "sum", "suser_sname", "switchoffset",
    "sysdatetime", "sysdatetimeoffset", "sysutcdatetime", "tan", "todatetimeoffset", "translate", "trim", "try_cast",
    "try_convert", "try_parse", "unicode", "upper", "user_name", "var", "year",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_batches_at_go_lines() {
        let script = "CREATE TABLE t (a int)\nGO\nINSERT t VALUES (1) -- GO here stays\ngo 2\nSELECT 'x\nGO\n'\n/* GO\n*/\nSELECT 1";
        let batches = split_batches(script);
        assert_eq!(batches.len(), 3, "{batches:?}");
        assert_eq!(batches[0], ("CREATE TABLE t (a int)\n".to_string(), 1));
        assert_eq!(batches[1].1, 2);
        assert!(batches[2].0.contains("SELECT 'x\nGO\n'") && batches[2].0.contains("SELECT 1"));
        assert_eq!(go_line("GOTO x"), None);
    }

    #[test]
    fn reads_words_outside_strings_and_comments() {
        let found: Vec<String> = words("SELECT [DELETE], 'DROP' /* EXEC /* nested */ INSERT */ FROM t -- UPDATE\nWHERE (x IN (SELECT 1))")
            .into_iter()
            .map(|(word, depth)| format!("{word}{depth}"))
            .collect();
        assert_eq!(found, ["SELECT0", "FROM0", "T0", "WHERE0", "X1", "IN1", "SELECT2"]);
    }

    #[test]
    fn checks_reads() {
        assert!(check_read_query("SELECT TOP 5 * FROM dbo.users ORDER BY id").is_ok());
        assert!(check_read_query("WITH a AS (SELECT 1 AS x) SELECT x FROM a").is_ok());
        assert!(check_read_query("SELECT [delete] FROM t WHERE note = 'drop table'").is_ok());
        for sql in [
            "DELETE FROM t",
            "SELECT 1 DELETE FROM t",
            "SELECT * INTO copy FROM t",
            "SELECT 1; COMMIT; DELETE FROM t",
            "SELECT * FROM OPENQUERY(remote, 'DELETE x')",
            "WITH a AS (SELECT 1 AS x) DELETE FROM t",
            "SELECT 1; SELECT 2",
            "SELECT 1 WAITFOR DELAY '01:00'",
            "EXEC sp_who",
        ] {
            assert!(check_read_query(sql).is_err(), "not refused: {sql}");
        }
    }

    #[test]
    fn sees_batches_without_rows() {
        assert!(returns_no_rows("UPDATE t SET a = 1; DELETE FROM t WHERE a = 2"));
        assert!(returns_no_rows("INSERT INTO t (a) SELECT a FROM s"));
        assert!(returns_no_rows("CREATE PROCEDURE p AS SELECT 1"));
        assert!(!returns_no_rows("UPDATE t SET a = 1 SELECT * FROM t"));
        assert!(!returns_no_rows("DELETE FROM t OUTPUT deleted.*"));
        assert!(!returns_no_rows("SELECT 1"));
    }

    #[test]
    fn writes_types_and_defaults() {
        assert_eq!(type_text("nvarchar", "nvarchar", "100", "0", "0"), "nvarchar(50)");
        assert_eq!(type_text("varchar", "varchar", "-1", "0", "0"), "varchar(max)");
        assert_eq!(type_text("decimal", "decimal", "9", "10", "2"), "decimal(10,2)");
        assert_eq!(type_text("datetime2", "datetime2", "8", "27", "7"), "datetime2");
        assert_eq!(type_text("Phone", "nvarchar", "40", "0", "0"), "Phone");
        assert_eq!(default_text("((0))"), "0");
        assert_eq!(default_text("(getdate())"), "getdate()");
        assert_eq!(default_text("((1)+(2))"), "(1)+(2)");
        assert_eq!(default_text("(N'a(b')"), "N'a(b'");
        assert_eq!(version_text("16.0.4135.4", "3"), "SQL Server 2022 (16.0.4135.4)");
        assert_eq!(version_text("12.0.2000.8", "5"), "Azure SQL 12.0.2000.8");
    }

    #[test]
    fn writes_structure_changes() {
        let relation = Relation { schema: "dbo".into(), name: "users".into(), kind: RelationKind::Table };
        let original = ColumnDetail { name: "age".into(), column_type: "int".into(), nullable: true, default_value: Some("0".into()), comment: None };
        let column = ColumnSpec { name: "years".into(), column_type: "bigint".into(), nullable: false, default_value: Some("1".into()), comment: None };
        let facts = CatalogFacts { default_constraint: Some("DF_users_age".into()), index_is_constraint: false };
        let statements = structure_change_sql(&relation, &StructureChange::EditColumn { original, column }, &facts).unwrap();
        assert_eq!(
            statements,
            [
                "ALTER TABLE [dbo].[users] DROP CONSTRAINT [DF_users_age];",
                "ALTER TABLE [dbo].[users] ALTER COLUMN [age] bigint NOT NULL;",
                "ALTER TABLE [dbo].[users] ADD DEFAULT 1 FOR [age];",
                "EXEC sp_rename N'[dbo].[users].[age]', N'years', 'COLUMN';",
            ]
        );
        let drop = structure_change_sql(&relation, &StructureChange::DropColumn { name: "age".into(), cascade: false }, &facts).unwrap();
        assert_eq!(drop.len(), 2);
    }
}

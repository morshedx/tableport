//! libSQL and Turso: SQLite on a server, reached with the Hrana protocol,
//! JSON over HTTPS. The SQL, the catalog and the screens are SQLite's; only
//! the way statements travel differs.
//!
//! A plain Hrana pipeline runs every request even after one fails, which
//! would COMMIT after a failed statement. Everything that must be all or
//! nothing runs on a stream instead: one request at a time, sharing the
//! server-side connection through its baton, so TablePort sees each result
//! and ends with COMMIT or ROLLBACK itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::rows::Column;
use crate::schema::Relation;
use crate::session::ConnectionSettings;
use crate::sqlite::{csv_field, is_numeric_type, real};
use crate::DatabaseError;

#[derive(Clone)]
pub(crate) struct LibsqlLink {
    client: reqwest::Client,
    /// The server's pipeline endpoint: `https://…/v2/pipeline`.
    pipeline_url: String,
    token: Option<String>,
    /// "libSQL 3.45.1".
    pub(crate) version: String,
    /// Hrana has no read-only mode: TablePort refuses writes itself and
    /// rolls back whatever the editor runs.
    read_only: bool,
    endpoint: String,
    closed: Arc<AtomicBool>,
}

/// One statement's outcome.
struct Outcome {
    columns: Vec<String>,
    /// Declared types, where the column has one.
    declared: Vec<Option<String>>,
    rows: Vec<Vec<Value>>,
    affected: u64,
}

impl LibsqlLink {
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let url = settings.database.trim().trim_end_matches('/');
        let base = if let Some(rest) = url.strip_prefix("libsql://").or_else(|| url.strip_prefix("wss://")) {
            format!("https://{rest}")
        } else if let Some(rest) = url.strip_prefix("ws://") {
            format!("http://{rest}")
        } else if url.starts_with("https://") || url.starts_with("http://") {
            url.to_string()
        } else {
            return Err(DatabaseError::new("A libSQL URL starts with libsql://, https:// or http://."));
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| DatabaseError::new(format!("Could not set up HTTPS: {error}")))?;
        let mut link = Self {
            client,
            pipeline_url: format!("{base}/v2/pipeline"),
            token: settings.password.clone().filter(|token| !token.is_empty()),
            version: String::new(),
            read_only: settings.read_only,
            endpoint: settings.endpoint(),
            closed: Arc::new(AtomicBool::new(false)),
        };
        let version = link.text_rows("SELECT sqlite_version()".into()).await?;
        let version = version.into_iter().next().and_then(|row| row.into_iter().next().flatten()).unwrap_or_default();
        link.version = format!("libSQL {version}");
        Ok(link)
    }

    fn stream(&self) -> Stream<'_> {
        Stream { link: self, baton: None }
    }

    /// One statement on a stream of its own.
    async fn one(&self, sql: &str) -> Result<Outcome, DatabaseError> {
        let mut stream = self.stream();
        let outcome = stream.execute(sql).await;
        stream.close().await;
        outcome
    }

    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        let outcome = self.one(&sql).await?;
        Ok(outcome.rows.into_iter().map(|row| row.iter().map(text).collect()).collect())
    }

    fn refuse_writes(&self) -> Result<(), DatabaseError> {
        if self.read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was changed."));
        }
        Ok(())
    }

    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        // Hrana takes one statement per request.
        let statements = crate::sql_text::statements(&sql);
        if statements.len() == 1 {
            return self.one(&statements[0]).await.map(|_| ());
        }
        self.apply_checked(statements, 0).await
    }

    /// Runs `statements` in one transaction: all or none. Each of the first
    /// `checked` must change a row; if one changes none, someone else
    /// changed or deleted it, and nothing is saved.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut stream = self.stream();
        let result = async {
            stream.execute("BEGIN").await?;
            for (index, statement) in statements.iter().enumerate() {
                let outcome = stream.execute(statement).await?;
                if index < checked && outcome.affected == 0 {
                    return Err(DatabaseError::new(
                        "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.",
                    ));
                }
            }
            stream.execute("COMMIT").await.map(|_| ())
        }
        .await;
        stream.end(result).await
    }

    /// A table rebuild, as for a file: foreign keys off around one
    /// transaction, then a check that every foreign key still finds its row.
    pub(crate) async fn rebuild(&self, rebuild: crate::sqlite_rebuild::Rebuild, schema: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut stream = self.stream();
        let result = async {
            let enabled = stream.execute("PRAGMA foreign_keys").await?;
            let was_on = enabled.rows.first().and_then(|row| row.first()).map(text) == Some(Some("1".into()));
            stream.execute("PRAGMA foreign_keys = OFF").await?;
            let inner = async {
                stream.execute("BEGIN").await?;
                for statement in &rebuild.statements {
                    stream.execute(statement).await?;
                }
                for table in &rebuild.checked {
                    let check = format!(
                        "SELECT count(*) FROM pragma_foreign_key_check({}, {})",
                        crate::rows::quote_literal(table)?,
                        crate::rows::quote_literal(&schema)?
                    );
                    let count = stream.execute(&check).await?;
                    let violations = count.rows.first().and_then(|row| row.first()).and_then(text).unwrap_or_default();
                    if violations != "0" {
                        return Err(DatabaseError::new(format!(
                            "{violations} rows of {table} would point at rows that don't exist. Nothing was changed."
                        )));
                    }
                }
                stream.execute("COMMIT").await.map(|_| ())
            }
            .await;
            let inner = match inner {
                Ok(()) => Ok(()),
                Err(error) => {
                    stream.execute("ROLLBACK").await.ok();
                    Err(error)
                }
            };
            if was_on {
                stream.execute("PRAGMA foreign_keys = ON").await?;
            }
            inner
        }
        .await;
        stream.close().await;
        result
    }

    /// The query editor's Run: each statement in turn, as one transaction
    /// unless the SQL manages its own.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{cut, QueryResult, ResultSet};
        let statements = crate::sql_text::statements(&sql);
        let verbs = crate::sql_text::statement_verbs(&sql);
        let manages_transaction =
            verbs.iter().any(|verb| matches!(verb.as_str(), "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"));
        if self.read_only {
            if let Some(verb) = verbs.iter().find(|verb| !matches!(verb.as_str(), "SELECT" | "WITH" | "VALUES" | "EXPLAIN")) {
                return Err(DatabaseError::new(format!("This connection is read-only: {verb} isn't run.")));
            }
        }
        let start = std::time::Instant::now();
        let mut stream = self.stream();
        let mut result_sets = Vec::new();
        let mut statement_counts = Vec::new();
        let result = async {
            if !manages_transaction {
                stream.execute("BEGIN").await?;
            }
            for statement in &statements {
                let outcome = stream.execute(statement).await?;
                if outcome.columns.is_empty() {
                    statement_counts.push(outcome.affected);
                    continue;
                }
                let total = outcome.rows.len() as u64;
                // Declared types first; for expressions, the first value's type.
                let numeric: Vec<bool> = (0..outcome.columns.len())
                    .map(|index| match outcome.declared.get(index).cloned().flatten() {
                        Some(declared) => is_numeric_type(&declared),
                        None => outcome
                            .rows
                            .iter()
                            .map(|row| row.get(index).and_then(|value| value["type"].as_str()).unwrap_or("null"))
                            .find(|kind| *kind != "null")
                            .is_some_and(|kind| kind == "integer" || kind == "float"),
                    })
                    .collect();
                let rows = outcome
                    .rows
                    .iter()
                    .take(usize::try_from(max_rows).unwrap_or(usize::MAX))
                    .map(|row| row.iter().map(|value| text(value).as_deref().map(cut)).collect())
                    .collect();
                statement_counts.push(total);
                result_sets.push(ResultSet { columns: outcome.columns, rows, total_rows: total, numeric_columns: numeric });
            }
            if self.read_only {
                // Whatever a read-only connection's SQL did, it doesn't stay.
                stream.execute("ROLLBACK").await?;
            } else if !manages_transaction {
                stream.execute("COMMIT").await?;
            }
            Ok(())
        }
        .await;
        stream.end(result).await?;
        Ok(QueryResult {
            result_sets,
            statement_counts,
            statement_verbs: verbs,
            elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    pub(crate) async fn export(
        &self,
        select: String,
        columns: Vec<Column>,
        relation: Relation,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        use crate::export::ExportFormat;
        use std::fmt::Write as _;
        let outcome = self.one(&select).await?;
        let target = relation.quoted_name();
        let names = if columns.is_empty() { outcome.columns.clone() } else { columns.iter().map(|column| column.name.clone()).collect() };
        let quoted_names = names.iter().map(|name| crate::rows::quote_identifier(name)).collect::<Vec<_>>().join(", ");
        let mut out = String::new();
        match format {
            ExportFormat::Csv => {
                let header = outcome.columns.iter().map(|name| csv_field(name)).collect::<Vec<_>>().join(",");
                out.push_str(&header);
                out.push('\n');
                for row in &outcome.rows {
                    let line = row.iter().map(|value| text(value).as_deref().map(csv_field).unwrap_or_default()).collect::<Vec<_>>();
                    out.push_str(&line.join(","));
                    out.push('\n');
                }
            }
            ExportFormat::Json => {
                out.push('[');
                for (index, row) in outcome.rows.iter().enumerate() {
                    let object: serde_json::Map<String, Value> = outcome.columns.iter().cloned().zip(row.iter().map(typed)).collect();
                    out.push_str(if index == 0 { "\n  " } else { ",\n  " });
                    out.push_str(&Value::Object(object).to_string());
                }
                out.push_str(if outcome.rows.is_empty() { "]\n" } else { "\n]\n" });
            }
            ExportFormat::Sql => {
                for row in &outcome.rows {
                    let literals = row.iter().map(literal).collect::<Result<Vec<_>, _>>()?;
                    writeln!(out, "INSERT INTO {target} ({quoted_names}) VALUES ({});", literals.join(", "))
                        .map_err(|error| DatabaseError::new(error.to_string()))?;
                }
            }
        }
        tokio::fs::write(&path, out).await.map_err(|error| DatabaseError::new(format!("Can't write {path}: {error}.")))?;
        Ok(outcome.rows.len() as u64)
    }

    /// One statement for AI access: a SELECT around it, in a transaction that
    /// is always rolled back. libSQL refuses a second statement in a request.
    pub(crate) async fn json_rows(&self, sql: String) -> Result<Vec<Value>, DatabaseError> {
        let mut stream = self.stream();
        let result = async {
            stream.execute("BEGIN").await?;
            stream.execute(&sql).await
        }
        .await;
        stream.execute("ROLLBACK").await.ok();
        stream.close().await;
        let outcome = result?;
        Ok(outcome
            .rows
            .iter()
            .map(|row| Value::Object(outcome.columns.iter().cloned().zip(row.iter().map(typed)).collect()))
            .collect())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

/// A server-side connection, kept between requests by its baton.
struct Stream<'a> {
    link: &'a LibsqlLink,
    baton: Option<String>,
}

impl Stream<'_> {
    async fn send(&mut self, requests: Vec<Value>) -> Result<Vec<Value>, DatabaseError> {
        let link = self.link;
        if link.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", link.endpoint)));
        }
        let mut request = link.client.post(&link.pipeline_url).header("content-type", "application/json");
        if let Some(token) = &link.token {
            request = request.bearer_auth(token);
        }
        let body = json!({ "baton": self.baton, "requests": requests });
        let response = request.body(body.to_string()).send().await.map_err(|error| reach_error(&link.endpoint, &error))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| reach_error(&link.endpoint, &error))?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DatabaseError::new(format!("{} refused the token. Check the auth token.", link.endpoint)));
        }
        if !status.is_success() {
            return Err(DatabaseError::new(format!("{} answered {status}: {}", link.endpoint, text.trim())));
        }
        let reply: Value = serde_json::from_str(&text).map_err(|error| DatabaseError::new(format!("Unreadable reply: {error}")))?;
        self.baton = reply["baton"].as_str().map(str::to_string);
        Ok(reply["results"].as_array().cloned().unwrap_or_default())
    }

    async fn execute(&mut self, sql: &str) -> Result<Outcome, DatabaseError> {
        let results = self.send(vec![json!({ "type": "execute", "stmt": { "sql": sql, "want_rows": true } })]).await?;
        let result = results.into_iter().next().unwrap_or(Value::Null);
        if result["type"] == "error" {
            let message = result["error"]["message"].as_str().unwrap_or("The statement failed.");
            return Err(DatabaseError::new(server_message(message)));
        }
        let result = &result["response"]["result"];
        let columns = result["cols"].as_array().cloned().unwrap_or_default();
        Ok(Outcome {
            columns: columns.iter().map(|column| column["name"].as_str().unwrap_or_default().to_string()).collect(),
            declared: columns.iter().map(|column| column["decltype"].as_str().map(str::to_string)).collect(),
            rows: result["rows"]
                .as_array()
                .map(|rows| rows.iter().map(|row| row.as_array().cloned().unwrap_or_default()).collect())
                .unwrap_or_default(),
            affected: result["affected_row_count"].as_u64().unwrap_or(0),
        })
    }

    /// Ends the stream after `result`: a failure rolls back what it started.
    async fn end(mut self, result: Result<(), DatabaseError>) -> Result<(), DatabaseError> {
        if result.is_err() {
            // A stream that never began a transaction has nothing to roll back.
            self.execute("ROLLBACK").await.ok();
        }
        self.close().await;
        result
    }

    async fn close(mut self) {
        if self.baton.is_some() {
            // The server closes a stream left open on its own, after a timeout.
            self.send(vec![json!({ "type": "close" })]).await.ok();
        }
    }
}

fn reach_error(endpoint: &str, error: &reqwest::Error) -> DatabaseError {
    if error.is_timeout() {
        return DatabaseError::new(format!("Could not reach {endpoint}: no answer within the time limit."));
    }
    if error.is_connect() {
        return DatabaseError::new(format!("Could not reach {endpoint}: Connection refused."));
    }
    DatabaseError::new(format!("Could not reach {endpoint}: {error}"))
}

/// The server's message without its prefix: "SQLite error: no such table: x".
fn server_message(message: &str) -> String {
    message.strip_prefix("SQLite error: ").unwrap_or(message).to_string()
}

/// A Hrana value as the grid shows it: numbers as SQLite prints them, blobs
/// as `\x…` hex.
fn text(value: &Value) -> Option<String> {
    match value["type"].as_str()? {
        "null" => None,
        "integer" => value["value"].as_str().map(str::to_string),
        "float" => value["value"].as_f64().map(real),
        "text" => value["value"].as_str().map(str::to_string),
        "blob" => Some(format!("\\x{}", base64_bytes(value["base64"].as_str().unwrap_or_default()).iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
        _ => None,
    }
}

/// A Hrana value as JSON with its type: numbers stay numbers.
fn typed(value: &Value) -> Value {
    match value["type"].as_str() {
        Some("integer") => value["value"].as_str().and_then(|number| number.parse::<serde_json::Number>().ok()).map_or(Value::Null, Value::Number),
        Some("float") => value["value"].clone(),
        _ => text(value).map_or(Value::Null, Value::String),
    }
}

/// A Hrana value as a SQLite literal.
fn literal(value: &Value) -> Result<String, DatabaseError> {
    Ok(match value["type"].as_str() {
        Some("null") | None => "NULL".to_string(),
        Some("integer" | "float") => text(value).unwrap_or_else(|| "NULL".into()),
        Some("blob") => format!(
            "X'{}'",
            base64_bytes(value["base64"].as_str().unwrap_or_default()).iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        ),
        Some(_) => crate::rows::quote_literal(&text(value).unwrap_or_default())?,
    })
}

/// Standard base64, with or without padding, as Hrana sends blobs.
fn base64_bytes(text: &str) -> Vec<u8> {
    let value = |character: u8| -> Option<u32> {
        match character {
            b'A'..=b'Z' => Some(u32::from(character - b'A')),
            b'a'..=b'z' => Some(u32::from(character - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(character - b'0') + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for sextet in text.bytes().filter_map(value) {
        buffer = (buffer << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

/// A SQLite connection: a file on this Mac, or a libSQL server. The SQL,
/// catalog and screens are the same; this passes each call to the right one.
#[derive(Clone)]
pub(crate) enum SqliteLike {
    File(crate::sqlite::SqliteLink),
    Remote(LibsqlLink),
}

impl SqliteLike {
    pub(crate) async fn text_rows(&self, sql: String) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        match self {
            Self::File(link) => link.text_rows(sql).await,
            Self::Remote(link) => link.text_rows(sql).await,
        }
    }

    pub(crate) async fn execute(&self, sql: String) -> Result<(), DatabaseError> {
        match self {
            Self::File(link) => link.execute(sql).await,
            Self::Remote(link) => link.execute(sql).await,
        }
    }

    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        match self {
            Self::File(link) => link.apply_checked(statements, checked).await,
            Self::Remote(link) => link.apply_checked(statements, checked).await,
        }
    }

    pub(crate) async fn rebuild(&self, rebuild: crate::sqlite_rebuild::Rebuild, schema: String) -> Result<(), DatabaseError> {
        match self {
            Self::File(link) => link.rebuild(rebuild, schema).await,
            Self::Remote(link) => link.rebuild(rebuild, schema).await,
        }
    }

    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        match self {
            Self::File(link) => link.run_query(sql, max_rows).await,
            Self::Remote(link) => link.run_query(sql, max_rows).await,
        }
    }

    pub(crate) async fn export(
        &self,
        select: String,
        columns: Vec<Column>,
        relation: Relation,
        format: crate::export::ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        match self {
            Self::File(link) => link.export(select, columns, relation, format, path).await,
            Self::Remote(link) => link.export(select, columns, relation, format, path).await,
        }
    }

    pub(crate) async fn json_rows(&self, sql: String) -> Result<Vec<Value>, DatabaseError> {
        match self {
            Self::File(link) => link.json_rows(sql).await,
            Self::Remote(link) => link.json_rows(sql).await,
        }
    }

    /// Stops the running statement; a libSQL server has no way to.
    pub(crate) fn interrupt(&self) -> Result<(), DatabaseError> {
        match self {
            Self::File(link) => {
                link.interrupt();
                Ok(())
            }
            Self::Remote(_) => Err(DatabaseError::new("A libSQL server can't stop a running statement.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_hrana_values() {
        assert_eq!(base64_bytes("yv4"), [0xca, 0xfe]);
        assert_eq!(base64_bytes("yv4="), [0xca, 0xfe]);
        assert_eq!(text(&json!({ "type": "blob", "base64": "yv4" })).as_deref(), Some("\\xcafe"));
        assert_eq!(text(&json!({ "type": "float", "value": 1.0 })).as_deref(), Some("1.0"));
        assert_eq!(text(&json!({ "type": "integer", "value": "9007199254740993" })).as_deref(), Some("9007199254740993"));
        assert_eq!(typed(&json!({ "type": "integer", "value": "42" })), json!(42));
        assert_eq!(text(&json!({ "type": "null" })), None);
        assert_eq!(literal(&json!({ "type": "text", "value": "it's" })).unwrap(), "'it''s'");
    }
}

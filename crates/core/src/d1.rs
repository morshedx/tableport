//! Cloudflare D1: SQLite behind Cloudflare's HTTP API. The SQL, catalog and
//! screens are SQLite's, as for libSQL; only the transport differs.
//!
//! D1 refuses BEGIN and COMMIT and keeps no session between requests. Its
//! one all-or-nothing unit is a batch: every statement of one request runs
//! in one transaction, and a failing statement undoes the batch. So what
//! must be all or nothing goes in one batch, and a check that must stop it
//! is a statement that fails: `json()` of text that isn't JSON raises an
//! error, which SQL can do in a plain SELECT, where RAISE() can't be used.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::rows::Column;
use crate::schema::Relation;
use crate::session::ConnectionSettings;
use crate::sqlite::{csv_field, real};
use crate::DatabaseError;

/// Fails the batch when the statement before it changed no row.
const ROW_GUARD: &str = "SELECT CASE WHEN changes() = 0 THEN json('tableport: row changed') END";
const ROW_CHANGED: &str = "A row was changed or deleted by someone else. Nothing was saved; refresh and try again.";

#[derive(Clone)]
pub(crate) struct D1Link {
    client: reqwest::Client,
    /// …/accounts/{account}/d1/database/{database}/raw
    url: String,
    token: String,
    /// "D1 (SQLite 3.45.0)".
    pub(crate) version: String,
    /// D1 has no read-only session: TablePort refuses writes itself.
    read_only: bool,
    endpoint: String,
    closed: Arc<AtomicBool>,
}

/// One statement's outcome in a batch.
struct Outcome {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    changes: u64,
}

impl D1Link {
    /// `user` is the Cloudflare account ID, `database` the D1 database ID
    /// and `password` an API token with D1 rights.
    pub(crate) async fn open(settings: &ConnectionSettings) -> Result<Self, DatabaseError> {
        let account = settings.user.trim();
        let database = settings.database.trim();
        if account.is_empty() || database.is_empty() {
            return Err(DatabaseError::new("D1 needs the account ID and the database ID."));
        }
        let token = settings.password.clone().unwrap_or_default();
        if token.trim().is_empty() {
            return Err(DatabaseError::new("D1 needs an API token with D1 rights."));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| DatabaseError::new(format!("Could not set up HTTPS: {error}")))?;
        let mut link = Self {
            client,
            url: format!("https://api.cloudflare.com/client/v4/accounts/{account}/d1/database/{database}/raw"),
            token: token.trim().to_string(),
            version: String::new(),
            read_only: settings.read_only,
            endpoint: settings.endpoint(),
            closed: Arc::new(AtomicBool::new(false)),
        };
        let version = link.text_rows("SELECT sqlite_version()".into()).await?;
        let version = version.into_iter().next().and_then(|row| row.into_iter().next().flatten()).unwrap_or_default();
        link.version = format!("D1 (SQLite {version})");
        Ok(link)
    }

    /// Sends the statements as one batch: all of them run, or none.
    async fn batch(&self, statements: &[String]) -> Result<Vec<Outcome>, DatabaseError> {
        if self.is_closed() {
            return Err(DatabaseError::new(format!("The connection to {} is closed.", self.endpoint)));
        }
        let body = match statements {
            [single] => json!({ "sql": single }),
            _ => json!({ "batch": statements.iter().map(|sql| json!({ "sql": sql })).collect::<Vec<_>>() }),
        };
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| reach_error(&self.endpoint, &error))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| reach_error(&self.endpoint, &error))?;
        let reply: Value = serde_json::from_str(&text)
            .map_err(|_| DatabaseError::new(format!("{} answered {status}: {}", self.endpoint, text.trim())))?;
        if reply["success"] != Value::Bool(true) {
            let message = reply["errors"]
                .as_array()
                .and_then(|errors| errors.first())
                .and_then(|error| error["message"].as_str())
                .unwrap_or("D1 refused the request.");
            if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
                return Err(DatabaseError::new(format!("Cloudflare refused the token: {message}")));
            }
            return Err(DatabaseError::new(server_message(message)));
        }
        Ok(reply["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|item| Outcome {
                columns: item["results"]["columns"]
                    .as_array()
                    .map(|columns| columns.iter().map(|name| name.as_str().unwrap_or_default().to_string()).collect())
                    .unwrap_or_default(),
                rows: item["results"]["rows"]
                    .as_array()
                    .map(|rows| rows.iter().map(|row| row.as_array().cloned().unwrap_or_default()).collect())
                    .unwrap_or_default(),
                changes: item["meta"]["changes"].as_u64().unwrap_or(0),
            })
            .collect())
    }

    async fn one(&self, sql: &str) -> Result<Outcome, DatabaseError> {
        self.batch(&[sql.to_string()]).await?.into_iter().next().ok_or_else(|| DatabaseError::new("D1 returned no result."))
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
        self.batch(&statements_of(&sql)?).await.map(|_| ())
    }

    /// Runs `statements` as one batch: all or none. A guard after each of
    /// the first `checked` fails the batch if that statement changed no row.
    pub(crate) async fn apply_checked(&self, statements: Vec<String>, checked: usize) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut batch = Vec::new();
        for (index, statement) in statements.into_iter().enumerate() {
            batch.push(statement);
            if index < checked {
                batch.push(ROW_GUARD.into());
            }
        }
        match self.batch(&batch).await {
            Err(error) if checked > 0 && error.message().contains("malformed JSON") => Err(DatabaseError::new(ROW_CHANGED)),
            other => other.map(|_| ()),
        }
    }

    /// A table rebuild in one batch. D1 can't turn foreign keys off; it can
    /// defer them to the end of the batch, by when the new table holds the
    /// rows, and a guard for each table fails the batch if a key would
    /// point at a row that doesn't exist.
    pub(crate) async fn rebuild(&self, rebuild: crate::sqlite_rebuild::Rebuild, schema: String) -> Result<(), DatabaseError> {
        self.refuse_writes()?;
        let mut batch = vec!["PRAGMA defer_foreign_keys = ON".to_string()];
        batch.extend(rebuild.statements);
        for table in &rebuild.checked {
            batch.push(format!(
                "SELECT CASE WHEN (SELECT count(*) FROM pragma_foreign_key_check({}, {})) > 0 THEN json('tableport: dangling key') END",
                crate::rows::quote_literal(table)?,
                crate::rows::quote_literal(&schema)?
            ));
        }
        match self.batch(&batch).await {
            Err(error) if error.message().contains("malformed JSON") => {
                Err(DatabaseError::new("Some rows would point at rows that don't exist. Nothing was changed."))
            }
            other => other.map(|_| ()),
        }
    }

    /// The query editor's Run: the statements as one batch, so they all run
    /// or none do. D1 has no BEGIN or COMMIT to manage it otherwise.
    pub(crate) async fn run_query(&self, sql: String, max_rows: u64) -> Result<crate::query::QueryResult, DatabaseError> {
        use crate::query::{cut, QueryResult, ResultSet};
        let statements = statements_of(&sql)?;
        let verbs = crate::sql_text::statement_verbs(&sql);
        if self.read_only {
            if let Some(verb) = verbs.iter().find(|verb| !matches!(verb.as_str(), "SELECT" | "WITH" | "VALUES" | "EXPLAIN")) {
                return Err(DatabaseError::new(format!("This connection is read-only: {verb} isn't run.")));
            }
        }
        let start = std::time::Instant::now();
        let outcomes = self.batch(&statements).await?;
        let mut result_sets = Vec::new();
        let mut statement_counts = Vec::new();
        for outcome in outcomes {
            if outcome.columns.is_empty() {
                statement_counts.push(outcome.changes);
                continue;
            }
            let total = outcome.rows.len() as u64;
            // No declared types come back: the first value that isn't NULL decides.
            let numeric = (0..outcome.columns.len())
                .map(|index| outcome.rows.iter().filter_map(|row| row.get(index)).find(|value| !value.is_null()).is_some_and(Value::is_number))
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
                out.push_str(&outcome.columns.iter().map(|name| csv_field(name)).collect::<Vec<_>>().join(","));
                out.push('\n');
                for row in &outcome.rows {
                    out.push_str(&row.iter().map(|value| text(value).as_deref().map(csv_field).unwrap_or_default()).collect::<Vec<_>>().join(","));
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

    /// One query for AI access, wrapped in a SELECT. D1 splits the text it
    /// gets into statements at semicolons itself, so no semicolon may reach
    /// it: the wrapping alone couldn't keep a second statement out.
    pub(crate) async fn json_rows(&self, sql: String) -> Result<Vec<Value>, DatabaseError> {
        if sql.contains(';') {
            return Err(DatabaseError::new("On D1 a query for AI access may not contain a semicolon: send one statement, without one."));
        }
        let outcome = self.one(&sql).await?;
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

/// The statements of a script; D1 refuses the transaction statements a
/// batch already gives.
fn statements_of(sql: &str) -> Result<Vec<String>, DatabaseError> {
    let statements: Vec<String> =
        crate::sql_text::statements(sql).into_iter().map(|statement| statement.trim().to_string()).filter(|statement| !statement.is_empty()).collect();
    if let Some(verb) = crate::sql_text::statement_verbs(sql)
        .into_iter()
        .find(|verb| matches!(verb.as_str(), "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"))
    {
        return Err(DatabaseError::new(format!("D1 runs every Run as one transaction already and refuses {verb}; leave it out.")));
    }
    if statements.is_empty() {
        return Err(DatabaseError::new("There is no SQL to run."));
    }
    Ok(statements)
}

fn reach_error(endpoint: &str, error: &reqwest::Error) -> DatabaseError {
    if error.is_timeout() {
        return DatabaseError::new(format!("Could not reach {endpoint}: no answer within the time limit."));
    }
    DatabaseError::new(format!("Could not reach {endpoint}: {error}"))
}

/// D1's message without its prefixes: "D1_ERROR: no such table: x: SQLITE_ERROR" → "no such table: x".
fn server_message(message: &str) -> String {
    let message = message.strip_prefix("D1_ERROR: ").unwrap_or(message);
    message.strip_suffix(": SQLITE_ERROR").unwrap_or(message).to_string()
}

/// Bytes of a BLOB: D1 sends them as an array of numbers, or as a Node Buffer.
fn bytes(value: &Value) -> Option<Vec<u8>> {
    let items = value.as_array().or_else(|| value["data"].as_array())?;
    items.iter().map(|item| item.as_u64().and_then(|byte| u8::try_from(byte).ok())).collect()
}

/// A value as the grid shows it: numbers as SQLite prints them, blobs as `\x…` hex.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(bit) => Some(if *bit { "1" } else { "0" }.into()),
        Value::Number(number) => Some(match number.as_i64() {
            Some(integer) => integer.to_string(),
            None => number.as_f64().map(real).unwrap_or_else(|| number.to_string()),
        }),
        Value::String(text) => Some(text.clone()),
        other => bytes(other).map(|bytes| format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
    }
}

/// A value as JSON with its type: numbers stay numbers.
fn typed(value: &Value) -> Value {
    match value {
        Value::Null | Value::Number(_) | Value::String(_) => value.clone(),
        other => text(other).map_or(Value::Null, Value::String),
    }
}

/// A value as a SQLite literal.
fn literal(value: &Value) -> Result<String, DatabaseError> {
    Ok(match value {
        Value::Null => "NULL".into(),
        Value::Number(_) | Value::Bool(_) => text(value).unwrap_or_else(|| "NULL".into()),
        Value::String(text) => crate::rows::quote_literal(text)?,
        other => match bytes(other) {
            Some(bytes) => format!("X'{}'", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
            None => crate::rows::quote_literal(&other.to_string())?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_and_refuses_transactions() {
        assert_eq!(text(&json!(42)).as_deref(), Some("42"));
        assert_eq!(text(&json!(9.5)).as_deref(), Some("9.5"));
        assert_eq!(text(&json!([202, 254])).as_deref(), Some("\\xcafe"));
        assert_eq!(text(&json!({ "type": "Buffer", "data": [1] })).as_deref(), Some("\\x01"));
        assert_eq!(literal(&json!("it's")).unwrap(), "'it''s'");
        assert!(statements_of("BEGIN; UPDATE t SET a = 1; COMMIT").is_err());
        assert_eq!(statements_of("UPDATE t SET a = 1; SELECT 1;").unwrap().len(), 2);
        assert_eq!(server_message("D1_ERROR: no such table: x: SQLITE_ERROR"), "no such table: x");
    }
}

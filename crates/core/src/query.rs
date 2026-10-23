use std::time::Instant;

use futures_util::future::join_all;
use futures_util::StreamExt;
use tokio_postgres::types::{Kind, Type};
use tokio_postgres::{NoTls, SimpleQueryMessage};

use crate::rows::GRID_CELL_LIMIT;
use crate::{on_runtime, DatabaseError, DatabaseSession};

/// The rows of one statement that returns rows.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ResultSet {
    pub columns: Vec<String>,
    /// At most `max_rows` rows; cells are text, None for NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// How many rows the statement returned in all.
    pub total_rows: u64,
    /// One per column: integers, numeric, floats and money, which the grid
    /// right-aligns. All false when the types couldn't be read.
    pub numeric_columns: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct QueryResult {
    /// One per statement that returned rows, in order.
    pub result_sets: Vec<ResultSet>,
    /// Rows returned or changed, one per statement, in order.
    pub statement_counts: Vec<u64>,
    /// Each statement's first word ("SELECT", "UPDATE"), in the same order;
    /// read from the SQL, as the driver doesn't pass the command tag on.
    pub statement_verbs: Vec<String>,
    pub elapsed_ms: u64,
}

#[uniffi::export]
impl DatabaseSession {
    /// Runs the SQL the user typed: one or more statements, as one implicit
    /// transaction unless the SQL has its own BEGIN/COMMIT. Keeps the first
    /// `max_rows` rows of each result, so a huge SELECT doesn't fill memory.
    pub async fn run_query(&self, sql: String, max_rows: u64) -> Result<QueryResult, DatabaseError> {
        if sql.trim().is_empty() {
            return Err(DatabaseError::new("There is no SQL to run."));
        }
        if let Some(link) = self.sqlite() {
            return on_runtime(async move { link.run_query(sql, max_rows).await }).await;
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.run_query(sql, max_rows).await }).await;
        }
        if let Some(link) = self.mssql() {
            return on_runtime(async move { link.run_query(sql, max_rows).await }).await;
        }
        if self.engine() == crate::Engine::Mongo {
            return self.mongo_run(sql, max_rows).await;
        }
        let client = self.client()?;
        let endpoint = self.settings().endpoint();
        on_runtime(async move {
            let map = |error| DatabaseError::from_postgres(error, &endpoint);
            let start = Instant::now();
            let stream = client.simple_query_raw(&sql).await.map_err(map)?;
            futures_util::pin_mut!(stream);

            let mut result_sets: Vec<ResultSet> = Vec::new();
            let mut statement_counts = Vec::new();
            // The set that rows are being added to, between RowDescription and CommandComplete.
            let mut open_set = false;
            // The index of the statement each result set came from.
            let mut set_statements = Vec::new();
            while let Some(message) = stream.next().await {
                match message.map_err(map)? {
                    SimpleQueryMessage::RowDescription(columns) => {
                        result_sets.push(ResultSet {
                            columns: columns.iter().map(|column| column.name().to_string()).collect(),
                            rows: Vec::new(),
                            total_rows: 0,
                            numeric_columns: vec![false; columns.len()],
                        });
                        set_statements.push(statement_counts.len());
                        open_set = true;
                    }
                    SimpleQueryMessage::Row(row) => {
                        if let Some(set) = result_sets.last_mut().filter(|_| open_set) {
                            if (set.rows.len() as u64) < max_rows {
                                set.rows.push((0..row.len()).map(|index| row.get(index).map(cut)).collect());
                            }
                        }
                    }
                    SimpleQueryMessage::CommandComplete(count) => {
                        if open_set {
                            if let Some(set) = result_sets.last_mut() {
                                set.total_rows = count;
                            }
                            open_set = false;
                        }
                        statement_counts.push(count);
                    }
                    _ => {}
                }
            }
            let elapsed_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

            // The text protocol sends no column types. Describe the statements
            // that returned rows (all at once, one round trip) to find the
            // number columns. Nothing runs again: describing only plans.
            let statements = crate::sql_text::statements(&sql);
            if statements.len() == statement_counts.len() {
                let described = join_all(set_statements.iter().map(|&index| client.prepare(&statements[index]))).await;
                for (set, statement) in result_sets.iter_mut().zip(described) {
                    // A statement that can't be described on its own (an
                    // aborted transaction) keeps its columns left-aligned.
                    let Ok(statement) = statement else { continue };
                    if statement.columns().len() == set.columns.len() {
                        set.numeric_columns = statement.columns().iter().map(|column| is_numeric(column.type_())).collect();
                    }
                }
            }

            Ok(QueryResult {
                result_sets,
                statement_counts,
                statement_verbs: crate::sql_text::statement_verbs(&sql),
                elapsed_ms,
            })
        })
        .await
    }

    /// Asks the server to stop the statement running on this connection.
    /// The running call then fails with "canceling statement due to user request".
    pub async fn cancel_query(&self) -> Result<(), DatabaseError> {
        if let Some(link) = self.sqlite() {
            return link.interrupt();
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.cancel().await }).await;
        }
        if let Some(link) = self.mssql() {
            link.cancel();
            return Ok(());
        }
        let token = self.client()?.cancel_token();
        let tls = self.tls();
        let endpoint = self.settings().endpoint();
        on_runtime(async move {
            let result = match tls {
                Some(tls) => token.cancel_query(tls).await,
                None => token.cancel_query(NoTls).await,
            };
            result.map_err(|error| DatabaseError::from_postgres(error, &endpoint))
        })
        .await
    }
}

fn is_numeric(column_type: &Type) -> bool {
    match column_type.kind() {
        Kind::Domain(base) => is_numeric(base),
        _ => matches!(
            *column_type,
            Type::INT2 | Type::INT4 | Type::INT8 | Type::NUMERIC | Type::FLOAT4 | Type::FLOAT8 | Type::MONEY
        ),
    }
}

/// The first GRID_CELL_LIMIT characters, as for table pages.
pub(crate) fn cut(text: &str) -> String {
    match text.char_indices().nth(GRID_CELL_LIMIT as usize) {
        Some((index, _)) => text[..index].to_string(),
        None => text.to_string(),
    }
}

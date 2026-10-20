use std::time::Instant;

use futures_util::StreamExt;
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
        let client = self.client();
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
            while let Some(message) = stream.next().await {
                match message.map_err(map)? {
                    SimpleQueryMessage::RowDescription(columns) => {
                        result_sets.push(ResultSet {
                            columns: columns.iter().map(|column| column.name().to_string()).collect(),
                            rows: Vec::new(),
                            total_rows: 0,
                        });
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
            Ok(QueryResult {
                result_sets,
                statement_counts,
                statement_verbs: crate::sql_text::statement_verbs(&sql),
                elapsed_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            })
        })
        .await
    }

    /// Asks the server to stop the statement running on this connection.
    /// The running call then fails with "canceling statement due to user request".
    pub async fn cancel_query(&self) -> Result<(), DatabaseError> {
        let token = self.client().cancel_token();
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

/// The first GRID_CELL_LIMIT characters, as for table pages.
fn cut(text: &str) -> String {
    match text.char_indices().nth(GRID_CELL_LIMIT as usize) {
        Some((index, _)) => text[..index].to_string(),
        None => text.to_string(),
    }
}

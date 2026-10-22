//! The database core of tableport. The Swift app calls it through bindings
//! that UniFFI generates; nothing here knows about the user interface.
//!
//! Every call runs on this crate's own tokio runtime and returns errors as
//! [`DatabaseError`]. Nothing may panic across the boundary: a panic in a
//! spawned task comes back as an error instead.

mod changes;
mod completion;
mod error;
mod export;
mod mcp;
mod libsql;
mod mysql;
mod query;
mod rows;
mod schema;
mod session;
mod sql_text;
mod sqlite;
mod sqlite_rebuild;
mod ssh;
mod store;
mod structure;
mod url;

pub use changes::{changes_sql, insert_sql, CellDraft, NewRow, PendingChanges, RowUpdate, SqlFunction};
pub use completion::{rank_names, Completer, CompletionItem, CompletionKind, Completions, KeywordCase};
pub use error::DatabaseError;
pub use export::{rows_to_csv, rows_to_insert_sql, rows_to_json, ExportFormat, ExportScope};
pub use mcp::{run_mcp_bridge, start_mcp_server, McpConnection, McpHost, McpLogEntry, McpServer};
pub use query::{QueryResult, ResultSet};
pub use rows::{grid_cell_limit, Column, ColumnKind, FilterOperator, FilterTarget, RowFilter, RowPage, RowSort, GRID_CELL_LIMIT};
pub use schema::{
    drop_relation_sql, drop_routine_sql, qualified_name, refresh_materialized_view_sql, truncate_sql, ForeignKey, Relation,
    RelationKind, Routine, SchemaContents,
};
pub use url::{parse_connection_url, ParsedUrl};
pub use structure::{
    create_table_sql, structure_change_sql, ColumnDetail, ColumnSpec, ConstraintDetail, ConstraintKind, IndexDetail, NewTableColumn,
    StructureChange, TableStructure,
};
pub use sql_text::{format_json, format_sql, statement_range, TextRange};
pub use ssh::SshSettings;
pub use store::{HistoryEntry, LocalStore, SavedQuery};
pub use session::{test_connection, ConnectionSettings, DatabaseSession, Engine, TlsMode};

uniffi::setup_scaffolding!();

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

fn runtime() -> Result<&'static Runtime, DatabaseError> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("tableport-core")
                .enable_all()
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| DatabaseError::new("Could not start the database runtime."))
}

/// Runs `future` on the core's runtime. Swift polls the returned future from
/// its own executor, which has no tokio reactor, so the I/O must live here.
async fn on_runtime<T, F>(future: F) -> Result<T, DatabaseError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, DatabaseError>> + Send + 'static,
{
    runtime()?
        .spawn(future)
        .await
        .map_err(|error| DatabaseError::new(format!("Internal error in the database core: {error}")))?
}

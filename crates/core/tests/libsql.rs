//! libSQL tests need a libSQL server (sqld) with an empty database; they
//! drop every table in it. Run them as
//! `TABLEPORT_TEST_LIBSQL=http://127.0.0.1:8081 cargo test`; without it they
//! pass without running. One server is one database, so one test does it all
//! in order.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tableport_core::{
    CellDraft, ColumnSpec, ConnectionSettings, DatabaseSession, Engine, ExportFormat, ExportScope, FilterOperator, FilterTarget,
    KeywordCase, McpConnection, McpHost, McpLogEntry, NewRow, PendingChanges, Relation, RelationKind, RowFilter, RowUpdate,
    StructureChange, TlsMode,
};

fn settings(url: &str) -> ConnectionSettings {
    ConnectionSettings {
        host: String::new(),
        port: 0,
        database: url.into(),
        user: String::new(),
        password: None,
        tls: TlsMode::Disable,
        ssh: None,
        read_only: false,
        engine: Engine::Libsql,
    }
}

fn table(name: &str) -> Relation {
    Relation { schema: "main".into(), name: name.into(), kind: RelationKind::Table }
}

struct Host {
    settings: ConnectionSettings,
}

impl McpHost for Host {
    fn connections(&self) -> Vec<McpConnection> {
        vec![McpConnection { id: "turso".into(), name: "Turso".into(), database: self.settings.database.clone(), all_databases: false, tag: None, engine: Engine::Libsql }]
    }

    fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
        Some(self.settings.clone())
    }

    fn record(&self, _entry: McpLogEntry) {}
}

#[tokio::test]
async fn works_like_sqlite_over_hrana() {
    let Ok(url) = std::env::var("TABLEPORT_TEST_LIBSQL") else { return };
    let session = DatabaseSession::connect(settings(&url)).await.expect("connect");
    assert!(session.server_version().starts_with("libSQL 3."), "{}", session.server_version());

    // A clean database: users references teams, so it goes first.
    session.execute("DROP VIEW IF EXISTS named; DROP TABLE IF EXISTS users; DROP TABLE IF EXISTS teams".into()).await.unwrap();
    for schema in session.schemas().await.unwrap() {
        for relation in schema.relations {
            let kind = if relation.kind == RelationKind::View { "VIEW" } else { "TABLE" };
            session.execute(format!("DROP {kind} IF EXISTS \"{}\"", relation.name)).await.unwrap();
        }
    }
    session
        .execute(
            "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT CHECK (length(name) > 0), score REAL, avatar BLOB,
                                 team_id INTEGER REFERENCES teams (id));
             CREATE VIEW named AS SELECT name FROM users;
             INSERT INTO teams VALUES (1, 'Core'), (2, 'Apps');
             INSERT INTO users (name, score, avatar, team_id) VALUES ('Ada', 9.5, x'cafe', 1), ('Grace', 7, NULL, 2), ('alan', NULL, NULL, NULL);"
                .into(),
        )
        .await
        .unwrap();

    // Browse.
    let schemas = session.schemas().await.unwrap();
    let names: Vec<_> = schemas[0].relations.iter().map(|relation| relation.name.as_str()).collect();
    assert_eq!(names, ["named", "teams", "users"]);
    let columns = session.columns(table("users")).await.unwrap();
    assert_eq!(columns.iter().map(|column| column.name.as_str()).collect::<Vec<_>>(), ["id", "name", "score", "avatar", "team_id"]);
    assert_eq!(session.primary_key(table("users")).await.unwrap(), ["id"]);
    assert_eq!(session.foreign_keys(table("users")).await.unwrap()[0].referenced_relation.name, "teams");
    let key = vec!["id".to_string()];
    let page = session.rows(table("users"), columns.clone(), Vec::new(), None, key.clone(), 0, 10).await.unwrap();
    assert_eq!(page.rows[0], [Some("1".into()), Some("Ada".into()), Some("9.5".into()), Some("\\xcafe".into()), Some("1".into())]);
    assert_eq!(page.rows[1][2].as_deref(), Some("7.0"));
    let contains = RowFilter { target: FilterTarget::Column { name: "name".into() }, operator: FilterOperator::Contains, value: "A".into() };
    assert_eq!(session.row_count(table("users"), columns.clone(), vec![contains]).await.unwrap(), 3);

    // Commit: all or nothing.
    let value = |text: &str| CellDraft::Value { text: text.into() };
    let changes = PendingChanges {
        inserts: vec![NewRow { values: HashMap::from([("name".into(), value("Linus"))]) }],
        updates: vec![RowUpdate { key: vec!["2".into()], values: HashMap::from([("name".into(), value("Grace H."))]) }],
        deletes: vec![vec!["3".into()]],
    };
    session.commit(changes, table("users"), columns.clone(), key.clone()).await.unwrap();
    let stale = PendingChanges {
        updates: vec![
            RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
            RowUpdate { key: vec!["99".into()], values: HashMap::from([("name".into(), value("gone"))]) },
        ],
        ..Default::default()
    };
    assert!(session.commit(stale, table("users"), columns.clone(), key.clone()).await.unwrap_err().message().contains("someone else"));
    let names = session.run_query("SELECT name FROM users ORDER BY id".into(), 10).await.unwrap();
    let names: Vec<_> = names.result_sets[0].rows.iter().map(|row| row[0].clone().unwrap()).collect();
    assert_eq!(names, ["Ada", "Grace H.", "Linus"]);

    // Queries: one transaction; a failure rolls back the statements before it.
    let result = session.run_query("UPDATE users SET score = 1 WHERE score IS NULL; SELECT count(*) FROM users".into(), 10).await.unwrap();
    assert_eq!(result.statement_counts, [1, 1]);
    assert!(session.run_query("UPDATE users SET name = 'x'; SELECT * FROM nope".into(), 10).await.is_err());
    let unchanged = session.run_query("SELECT count(*) FROM users WHERE name = 'x'".into(), 1).await.unwrap();
    assert_eq!(unchanged.result_sets[0].rows[0][0].as_deref(), Some("0"));
    assert!(session.cancel_query().await.is_err(), "libSQL has no cancel");

    // Export.
    let path = std::env::temp_dir().join(format!("tableport-libsql-{}.json", std::process::id()));
    let count = session
        .export_rows(table("users"), columns.clone(), Vec::new(), None, key.clone(), ExportScope::All, ExportFormat::Json, path.to_string_lossy().into())
        .await
        .unwrap();
    assert_eq!(count, 3);
    let exported: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(exported[0], json!({ "id": 1, "name": "Ada", "score": 9.5, "avatar": "\\xcafe", "team_id": 1 }));
    std::fs::remove_file(&path).ok();

    // A rebuild, with a view on the table and the CHECK kept.
    let name = session.structure(table("users")).await.unwrap().columns.into_iter().find(|column| column.name == "name").unwrap();
    let edited = ColumnSpec { name: "name".into(), column_type: "VARCHAR(80)".into(), nullable: false, default_value: Some("'?'".into()), comment: None };
    session.apply_structure_change(table("users"), StructureChange::EditColumn { original: name, column: edited }).await.unwrap();
    let definition = session.run_query("SELECT sql FROM sqlite_master WHERE name = 'users'".into(), 1).await.unwrap();
    let definition = definition.result_sets[0].rows[0][0].clone().unwrap();
    assert!(definition.contains("VARCHAR(80)") && definition.contains("NOT NULL") && definition.contains("CHECK"), "{definition}");
    let view = session.run_query("SELECT count(*) FROM named".into(), 1).await.unwrap();
    assert_eq!(view.result_sets[0].rows[0][0].as_deref(), Some("3"), "the view came back");
    let enforced = session.execute("INSERT INTO users (name, team_id) VALUES ('x', 42)".into()).await.unwrap_err();
    assert!(enforced.message().contains("FOREIGN KEY"), "foreign keys are back on: {enforced}");

    // Autocomplete.
    let completer = session.completer().await.unwrap();
    let items = completer.complete("select * from us".into(), 16, false, KeywordCase::Upper).items;
    assert_eq!(items[0].label, "users");

    // AI access: reads work; every write fails, and nothing changes.
    let socket = std::env::temp_dir().join(format!("tableport-mcp-libsql-{}.sock", std::process::id()));
    let server = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), Arc::new(Host { settings: settings(&url) })).unwrap();
    let (reader, mut writer) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let ask = |tool: &str, sql: &str| {
        let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": { "sql": sql } } });
        async move { message }
    };
    macro_rules! call {
        ($tool:expr, $sql:expr) => {{
            let message = ask($tool, $sql).await;
            writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
            let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            (reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(), reply["result"]["isError"].as_bool().unwrap_or(true))
        }};
    }
    let (text, error) = call!("run_read_query", "SELECT id, name FROM users ORDER BY id");
    assert!(!error && text.contains("\"name\": \"Ada\""), "{text}");
    for sql in [
        "DELETE FROM users",
        "UPDATE users SET name = 'x'",
        "SELECT 1; DELETE FROM users",
        "SELECT 1) ; DELETE FROM users; SELECT * FROM (SELECT 1",
        "WITH gone AS (DELETE FROM users RETURNING *) SELECT * FROM gone",
        "ATTACH DATABASE '/tmp/x.sqlite' AS x",
        "PRAGMA foreign_keys = OFF",
    ] {
        let (text, error) = call!("run_read_query", sql);
        assert!(error, "not refused: {sql} → {text}");
    }
    server.stop();

    // A read-only connection refuses writes and keeps nothing a query does.
    let read_only = DatabaseSession::connect(ConnectionSettings { read_only: true, ..settings(&url) }).await.unwrap();
    assert!(read_only.run_query("DELETE FROM users".into(), 1).await.is_err());
    assert!(read_only.execute("DELETE FROM users".into()).await.is_err());
    assert!(read_only.run_query("SELECT count(*) FROM users".into(), 1).await.is_ok());
    let after = session.run_query("SELECT count(*) FROM users WHERE name <> 'x'".into(), 1).await.unwrap();
    assert_eq!(after.result_sets[0].rows[0][0].as_deref(), Some("3"));
}

mod common;

use std::sync::{Arc, Mutex};

use common::{enabled, local, with_schema};
use serde_json::{json, Value};
use tableport_core::{start_mcp_server, ConnectionSettings, McpConnection, McpHost, McpLogEntry, TlsMode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Host {
    log: Mutex<Vec<McpLogEntry>>,
}

impl McpHost for Host {
    fn connections(&self) -> Vec<McpConnection> {
        vec![
            McpConnection { id: "local-id".into(), name: "Local".into(), database: "postgres".into(), all_databases: false, tag: None },
            McpConnection { id: "server-id".into(), name: "Server".into(), database: "postgres".into(), all_databases: true, tag: Some("local".into()) },
        ]
    }

    fn settings(&self, connection_id: String) -> Option<ConnectionSettings> {
        ["local-id", "server-id"].contains(&connection_id.as_str()).then(|| local("postgres", 5432, TlsMode::Disable))
    }

    fn record(&self, entry: McpLogEntry) {
        self.log.lock().unwrap().push(entry);
    }
}

/// A client on the server's socket: one JSON-RPC request, one reply.
struct Client {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: i64,
}

impl Client {
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        self.writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        let line = self.lines.next_line().await.unwrap().expect("a reply");
        serde_json::from_str(&line).unwrap()
    }

    /// A tool's text and whether it was an error.
    async fn call(&mut self, tool: &str, arguments: Value) -> (String, bool) {
        let reply = self.request("tools/call", json!({ "name": tool, "arguments": arguments })).await;
        let result = &reply["result"];
        (result["content"][0]["text"].as_str().unwrap_or_default().to_string(), result["isError"].as_bool().unwrap_or(true))
    }
}

#[tokio::test]
async fn read_only_tools_over_the_socket() {
    if !enabled() { return; }
    with_schema(
        &[
            "CREATE TABLE $S.notes (id int PRIMARY KEY, body text)",
            "INSERT INTO $S.notes SELECT g, 'note ' || g FROM generate_series(1, 5) g",
            "CREATE FUNCTION $S.sneaky() RETURNS int LANGUAGE sql AS 'INSERT INTO $S.notes VALUES (99, ''x'') RETURNING id'",
        ],
        |session, schema| async move {
            let socket = std::env::temp_dir().join(format!("tableport-mcp-test-{}.sock", std::process::id()));
            let host = Arc::new(Host { log: Mutex::new(Vec::new()) });
            let server = start_mcp_server(socket.to_string_lossy().into(), "test".into(), host.clone()).unwrap();
            let (reader, writer) = UnixStream::connect(&socket).await.unwrap().into_split();
            let mut client = Client { lines: BufReader::new(reader).lines(), writer, next_id: 0 };

            let init = client.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} })).await;
            assert_eq!(init["result"]["serverInfo"]["name"], "tableport");
            let tools = client.request("tools/list", json!({})).await;
            assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);

            // Other databases: only where the user allowed the whole server.
            let (text, error) = client.call("list_databases", json!({ "connection": "Server" })).await;
            assert!(!error && text.contains("\"template1\"") == false && text.contains("\"postgres\""), "{text}");
            let (text, error) = client.call("run_read_query", json!({ "connection": "Local", "database": "template1", "sql": "SELECT 1 AS one" })).await;
            assert!(error && text.contains("Every database on this server"), "{text}");
            let (text, error) = client.call("run_read_query", json!({ "connection": "Server", "database": "template1", "sql": "SELECT current_database() AS db" })).await;
            assert!(!error && text.contains("template1"), "{text}");

            let (text, error) = client.call("run_read_query", json!({ "connection": "Local", "sql": format!("SELECT id, body FROM {schema}.notes ORDER BY id;"), "max_rows": 3 })).await;
            assert!(!error, "{text}");
            let body: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["rows"][0], json!({ "id": 1, "body": "note 1" }));
            assert_eq!((body["row_count"].as_u64(), body["truncated"].as_bool()), (Some(3), Some(true)));

            // Every way to write is refused, and nothing changes.
            for sql in [
                format!("DELETE FROM {schema}.notes"),
                format!("SELECT 1; DELETE FROM {schema}.notes"),
                format!("WITH gone AS (DELETE FROM {schema}.notes RETURNING *) SELECT * FROM gone"),
                format!("SELECT {schema}.sneaky()"),
                "SET TRANSACTION READ WRITE".to_string(),
            ] {
                let (text, error) = client.call("run_read_query", json!({ "connection": "Local", "sql": sql })).await;
                assert!(error, "not refused: {sql} → {text}");
            }
            // A comment can't close the wrapping: the rest of the line is
            // comment, so only SELECT 1 runs.
            let (text, error) = client.call("run_read_query", json!({ "connection": "Local", "sql": format!("SELECT 1 -- ) AS x; DELETE FROM {schema}.notes; --") })).await;
            assert!(!error && text.contains("\"?column?\": 1"), "{text}");
            let (text, _) = client.call("run_read_query", json!({ "connection": "Local", "sql": format!("delete from {schema}.notes") })).await;
            assert!(text.contains("DELETE isn't allowed"), "{text}");
            let rows = session.run_query(format!("SELECT count(*)::text FROM {schema}.notes"), 1).await.unwrap();
            assert_eq!(rows.result_sets[0].rows[0][0].as_deref(), Some("5"));

            // The session still works after the refusals.
            let (text, error) = client.call("describe_table", json!({ "connection": "Local", "schema": schema, "table": "notes" })).await;
            assert!(!error && text.contains("id integer NOT NULL") && text.contains("PRIMARY KEY (id)"), "{text}");
            let (text, error) = client.call("explain_query", json!({ "connection": "Local", "sql": format!("SELECT * FROM {schema}.notes WHERE id = 1") })).await;
            assert!(!error && text.contains("Scan"), "{text}");
            let (text, error) = client.call("run_read_query", json!({ "connection": "Nope", "sql": "SELECT 1" })).await;
            assert!(error && text.contains("list_connections"), "{text}");

            let log = host.log.lock().unwrap();
            assert_eq!(log.iter().filter(|entry| entry.tool == "run_read_query").count(), 11);
            assert!(log.iter().any(|entry| entry.succeeded && entry.summary == "3+ rows"));
            drop(log);
            server.stop();
            assert!(!socket.exists());
        },
    )
    .await;
}

/// One allowed SQLite file.
struct SqliteHost {
    path: String,
}

impl McpHost for SqliteHost {
    fn connections(&self) -> Vec<McpConnection> {
        vec![McpConnection { id: "file".into(), name: "File".into(), database: self.path.clone(), all_databases: false, tag: None }]
    }

    fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
        Some(ConnectionSettings {
            host: String::new(),
            port: 0,
            database: self.path.clone(),
            user: String::new(),
            password: None,
            tls: TlsMode::Disable,
            ssh: None,
            read_only: false,
            engine: tableport_core::Engine::Sqlite,
        })
    }

    fn record(&self, _entry: McpLogEntry) {}
}

/// The SQLite attack test: every way to write fails, and the file is unchanged.
#[tokio::test]
async fn sqlite_files_stay_read_only_for_ai_tools() {
    let path = std::env::temp_dir().join(format!("tableport-mcp-{}.sqlite", std::process::id()));
    std::fs::remove_file(&path).ok();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
             INSERT INTO notes (body) VALUES ('a'), ('b'), ('c');
             CREATE TRIGGER sneaky AFTER UPDATE ON notes BEGIN DELETE FROM notes; END;",
        )
        .unwrap();
    let socket = std::env::temp_dir().join(format!("tableport-mcp-sqlite-{}.sock", std::process::id()));
    let host = Arc::new(SqliteHost { path: path.to_string_lossy().into() });
    let server = start_mcp_server(socket.to_string_lossy().into(), "test".into(), host).unwrap();
    let (reader, writer) = UnixStream::connect(&socket).await.unwrap().into_split();
    let mut client = Client { lines: BufReader::new(reader).lines(), writer, next_id: 0 };

    let (text, error) = client.call("run_read_query", json!({ "sql": "SELECT id, body FROM notes ORDER BY id" })).await;
    assert!(!error, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["rows"][0], json!({ "id": 1, "body": "a" }));

    for sql in [
        "DELETE FROM notes",
        "SELECT 1; DELETE FROM notes",
        "WITH gone AS (DELETE FROM notes RETURNING *) SELECT * FROM gone",
        "PRAGMA query_only = OFF",
        "ATTACH DATABASE '/tmp/x.sqlite' AS x",
        "UPDATE notes SET body = 'x'",
    ] {
        let (text, error) = client.call("run_read_query", json!({ "sql": sql })).await;
        assert!(error, "not refused: {sql} → {text}");
    }
    let (text, error) = client.call("explain_query", json!({ "sql": "SELECT * FROM notes WHERE id = 1" })).await;
    assert!(!error && text.to_uppercase().contains("SEARCH"), "{text}");
    let (text, error) = client.call("describe_table", json!({ "schema": "main", "table": "notes" })).await;
    assert!(!error && text.contains("PRIMARY KEY (id)"), "{text}");

    server.stop();
    let count: i64 = rusqlite::Connection::open(&path).unwrap().query_row("SELECT count(*) FROM notes", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 3);
    std::fs::remove_file(&path).ok();
}

//! Redis tests need a Redis server they may empty database 5 of. Run them
//! as `TABLEPORT_TEST_REDIS=127.0.0.1:6380 cargo test`; without it they pass
//! without running.

use std::sync::Arc;

use serde_json::{json, Value};
use tableport_core::{
    redis_change_commands, ConnectionSettings, DatabaseSession, Engine, McpConnection, McpHost, McpLogEntry, RedisChange,
    RedisKeyKind, TlsMode,
};

const DB: u32 = 5;

fn settings(server: &str, read_only: bool) -> ConnectionSettings {
    let (host, port) = server.rsplit_once(':').expect("host:port");
    ConnectionSettings {
        host: host.into(),
        port: port.parse().expect("a port"),
        database: DB.to_string(),
        user: String::new(),
        password: None,
        tls: TlsMode::Prefer,
        ssh: None,
        read_only,
        engine: Engine::Redis,
    }
}

struct Host {
    settings: ConnectionSettings,
}

impl McpHost for Host {
    fn connections(&self) -> Vec<McpConnection> {
        vec![McpConnection { id: "cache".into(), name: "Cache".into(), database: DB.to_string(), all_databases: false, tag: None, engine: Engine::Redis }]
    }

    fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
        Some(self.settings.clone())
    }

    fn record(&self, _entry: McpLogEntry) {}
}

#[tokio::test]
async fn browses_edits_and_reads_keys() {
    let Ok(server) = std::env::var("TABLEPORT_TEST_REDIS") else { return };
    let session = DatabaseSession::connect(settings(&server, false)).await.expect("connect");
    assert!(session.server_version().starts_with("Redis ") || session.server_version().starts_with("Valkey "), "{}", session.server_version());
    let run = |line: &str| {
        let session = session.clone();
        let line = line.to_string();
        async move { session.redis_command(DB, line).await }
    };
    run("FLUSHDB").await.unwrap();
    for line in [
        "SET user:1:name Ada",
        "SET user:2:name \"Grace H\" EX 600",
        "HSET user:1 name Ada lang Rust",
        "RPUSH queue a b c",
        "SADD tags red green",
        "ZADD scores 9.5 ada 7 grace",
        "XADD events * kind login who ada",
        "SET bytes \"\\x00\\xff\"",
    ] {
        run(line).await.unwrap_or_else(|error| panic!("{line}: {error}"));
    }

    // Databases and keys.
    let databases = session.redis_databases().await.unwrap();
    assert!(databases.len() >= 16);
    assert_eq!(databases.iter().find(|db| db.index == DB).unwrap().keys, 8);
    let mut found = Vec::new();
    let mut cursor = 0;
    loop {
        let page = session.redis_scan(DB, "user:*".into(), cursor, 2).await.unwrap();
        found.extend(page.keys);
        cursor = page.cursor;
        if cursor == 0 {
            break;
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = found.iter().map(|key| (key.name.as_str(), key.kind)).collect();
    assert_eq!(names, [("user:1", RedisKeyKind::Hash), ("user:1:name", RedisKeyKind::String), ("user:2:name", RedisKeyKind::String)]);

    // Values by type.
    let value = |key: &str| {
        let session = session.clone();
        let key = key.to_string();
        async move { session.redis_value(DB, key, 100).await.unwrap() }
    };
    let grace = value("user:2:name").await;
    assert_eq!((grace.text.as_deref(), grace.length), (Some("Grace H"), 7));
    assert!(grace.ttl_seconds.is_some_and(|ttl| ttl > 590 && ttl <= 600));
    assert_eq!(value("user:1:name").await.ttl_seconds, None);
    let hash = value("user:1").await;
    let mut fields: Vec<_> = hash.items.iter().map(|item| (item.field.clone().unwrap(), item.value.clone())).collect();
    fields.sort();
    assert_eq!(fields, [("lang".to_string(), "Rust".to_string()), ("name".into(), "Ada".into())]);
    let list = value("queue").await;
    assert_eq!(list.items.iter().map(|item| item.value.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    let scores = value("scores").await;
    assert_eq!(scores.items.iter().map(|item| (item.value.as_str(), item.score)).collect::<Vec<_>>(), [("grace", Some(7.0)), ("ada", Some(9.5))]);
    let events = value("events").await;
    assert_eq!(events.kind, RedisKeyKind::Stream);
    assert!(events.items[0].value.contains("\"who\":\"ada\""), "{:?}", events.items);
    assert_eq!(value("bytes").await.text.as_deref(), Some("\\x00\\xff"));
    assert_eq!(value("tags").await.length, 2);

    // Changes, reviewed as commands, then run together.
    let changes = vec![
        RedisChange::SetString { key: "user:2:name".into(), value: "Grace Hopper".into() },
        RedisChange::SetField { key: "user:1".into(), field: "lang".into(), value: "Swift".into() },
        RedisChange::DeleteField { key: "user:1".into(), field: "name".into() },
        RedisChange::SetListItem { key: "queue".into(), index: 0, value: "A".into() },
        RedisChange::RemoveListItem { key: "queue".into(), value: "b".into() },
        RedisChange::PushListItem { key: "queue".into(), value: "d".into() },
        RedisChange::AddMember { key: "scores".into(), member: "linus".into(), score: Some(8.0) },
        RedisChange::RemoveMember { key: "tags".into(), member: "red".into(), sorted: false },
        RedisChange::SetTtl { key: "user:1:name".into(), seconds: Some(100) },
        RedisChange::RenameKey { key: "events".into(), new_name: "log".into() },
    ];
    let lines = redis_change_commands(changes.clone()).unwrap();
    assert_eq!(lines[0], "SET user:2:name \"Grace Hopper\" KEEPTTL");
    session.redis_apply(DB, changes).await.unwrap();
    assert_eq!(value("user:2:name").await.text.as_deref(), Some("Grace Hopper"));
    assert!(value("user:2:name").await.ttl_seconds.is_some(), "KEEPTTL kept the expiry");
    assert_eq!(value("user:1").await.items.len(), 1);
    assert_eq!(value("queue").await.items.iter().map(|item| item.value.as_str()).collect::<Vec<_>>(), ["A", "c", "d"]);
    assert_eq!(value("scores").await.length, 3);
    assert!(value("user:1:name").await.ttl_seconds.is_some());
    assert_eq!(value("log").await.kind, RedisKeyKind::Stream);
    session.redis_apply(DB, vec![RedisChange::SetTtl { key: "user:1:name".into(), seconds: None }, RedisChange::DeleteKey { key: "log".into() }]).await.unwrap();
    assert_eq!(value("user:1:name").await.ttl_seconds, None);
    assert!(session.redis_value(DB, "log".into(), 10).await.is_err());

    // The console: redis-cli's replies; commands that would move the shared connection are refused.
    assert_eq!(run("LRANGE queue 0 -1").await.unwrap(), "1) \"A\"\n2) \"c\"\n3) \"d\"");
    assert_eq!(run("GET nothing").await.unwrap(), "(nil)");
    assert_eq!(run("SCARD tags").await.unwrap(), "(integer) 1");
    assert!(run("SELECT 0").await.unwrap_err().message().contains("sidebar"));
    assert!(run("MULTI").await.is_err());
    assert!(run("NOSUCHCOMMAND").await.is_err());

    // A read-only connection reads, and refuses every write.
    let read_only = DatabaseSession::connect(settings(&server, true)).await.unwrap();
    assert!(read_only.redis_command(DB, "GET user:1:name".into()).await.is_ok());
    assert!(read_only.redis_command(DB, "DEL user:1:name".into()).await.is_err());
    assert!(read_only.redis_apply(DB, vec![RedisChange::DeleteKey { key: "queue".into() }]).await.is_err());

    // AI access: read commands only.
    let socket = std::env::temp_dir().join(format!("tableport-mcp-redis-{}.sock", std::process::id()));
    let mcp = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), Arc::new(Host { settings: settings(&server, false) })).unwrap();
    let (reader, mut writer) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut lines = tokio::io::BufReader::new(reader).lines();
    macro_rules! call {
        ($tool:expr, $arguments:expr) => {{
            let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": $tool, "arguments": $arguments } });
            writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
            let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            (reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(), reply["result"]["isError"].as_bool().unwrap_or(true))
        }};
    }
    let (text, error) = call!("run_read_query", json!({ "sql": "HGETALL user:1" }));
    assert!(!error && text.contains("Swift"), "{text}");
    let (text, error) = call!("list_tables", json!({}));
    assert!(!error && text.contains("queue (list)"), "{text}");
    let (text, error) = call!("describe_table", json!({ "table": "scores" }));
    assert!(!error && text.contains("\"type\": \"zset\""), "{text}");
    let (text, error) = call!("list_connections", json!({}));
    assert!(!error && text.contains("\"engine\": \"Redis\""), "{text}");
    for line in [
        "SET user:1:name x",
        "DEL queue",
        "FLUSHDB",
        "KEYS *",
        "EVAL \"return redis.call('DEL', 'queue')\" 0",
        "FCALL wipe 0",
        "SORT_RO queue ALPHA STORE copy",
        "CONFIG SET maxmemory 1",
        "SELECT 0",
        "MULTI",
    ] {
        let (text, error) = call!("run_read_query", json!({ "sql": line }));
        assert!(error, "not refused: {line} → {text}");
    }
    mcp.stop();
    assert_eq!(run("DBSIZE").await.unwrap(), "(integer) 7");
    assert_eq!(run("GET user:1:name").await.unwrap(), "\"Ada\"");
    run("FLUSHDB").await.unwrap();
}

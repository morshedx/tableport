//! Cassandra tests need a server whose `cassandra` login (password
//! `cassandra`) may create keyspaces: `TABLEPORT_TEST_CASSANDRA=127.0.0.1:9043
//! cargo test`. Without it they pass without running. Each test works in a
//! keyspace of its own, dropped afterwards.

use std::collections::HashMap;
use std::sync::Arc;

use tableport_core::{ConnectionSettings, DatabaseSession, Engine, Relation, RelationKind, TlsMode};

fn settings(keyspace: &str) -> Option<ConnectionSettings> {
    let server = std::env::var("TABLEPORT_TEST_CASSANDRA").ok()?;
    let (host, port) = server.rsplit_once(':')?;
    Some(ConnectionSettings {
        host: host.into(),
        port: port.parse().ok()?,
        database: keyspace.into(),
        user: "cassandra".into(),
        password: Some("cassandra".into()),
        tls: TlsMode::Disable,
        ssh: None,
        read_only: false,
        engine: Engine::Cassandra,
    })
}

fn shop(keyspace: &str) -> Vec<String> {
    vec![
        format!("CREATE KEYSPACE {keyspace} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"),
        format!(
            "CREATE TABLE {keyspace}.users (team text, id int, name text, score decimal, joined date, seen timestamp, \
             active boolean, avatar blob, tags set<text>, prefs map<text, int>, code uuid, PRIMARY KEY ((team), id))"
        ),
        format!("CREATE TABLE {keyspace}.teams (id int PRIMARY KEY, name text)"),
        format!(
            "INSERT INTO {keyspace}.users (team, id, name, score, joined, seen, active, avatar, tags, prefs, code) VALUES \
             ('core', 1, 'Ada', 9.50, '2024-01-02', '2024-01-02 03:04:05+0000', true, 0xcafe, {{'a', 'b'}}, {{'x': 1}}, 5b6962dd-3f90-4c93-8f61-eabfa4a803e2)"
        ),
        format!("INSERT INTO {keyspace}.users (team, id, name, score, active) VALUES ('core', 2, 'Grace', 7, false)"),
        format!("INSERT INTO {keyspace}.users (team, id, name) VALUES ('apps', 3, 'alan')"),
        format!("INSERT INTO {keyspace}.users (team, id, name, score) VALUES ('apps', 4, '100% sure', 1)"),
        format!("INSERT INTO {keyspace}.users (team, id, name) VALUES ('apps', 5, 'Zoë ✓')"),
        format!("INSERT INTO {keyspace}.teams (id, name) VALUES (1, 'Core')"),
    ]
}

/// Runs `body` in a fresh keyspace that is dropped afterwards.
async fn in_keyspace<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<DatabaseSession>, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let keyspace = format!("tableport_{name}_{}", std::process::id());
    let Some(admin_settings) = settings("") else { return };
    let admin = DatabaseSession::connect(admin_settings).await.expect("connect");
    admin.execute(format!("DROP KEYSPACE IF EXISTS {keyspace}")).await.unwrap();
    for statement in shop(&keyspace) {
        admin.execute(statement.clone()).await.unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    let session = DatabaseSession::connect(settings(&keyspace).unwrap()).await.expect("connect to the keyspace");
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(session, keyspace.clone()))).await;
    admin.execute(format!("DROP KEYSPACE {keyspace}")).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn users(keyspace: &str) -> Relation {
    Relation { schema: keyspace.into(), name: "users".into(), kind: RelationKind::Table }
}

#[tokio::test]
async fn connects_and_names_the_server() {
    let Some(settings) = settings("") else { return };
    let session = DatabaseSession::connect(settings.clone()).await.unwrap();
    assert!(session.server_version().starts_with("Cassandra 5."), "{}", session.server_version());
    session.ping().await.unwrap();
    let wrong = ConnectionSettings { password: Some("wrong".into()), ..settings };
    let error = tableport_core::test_connection(wrong).await.unwrap_err();
    assert!(error.message().to_lowercase().contains("password") || error.message().contains("login"), "{error}");
}

#[tokio::test]
async fn browses_keyspaces_columns_and_keys() {
    in_keyspace("browse", |session, keyspace| async move {
        let schemas = session.schemas().await.unwrap();
        assert!(schemas.iter().all(|schema| !schema.name.starts_with("system")));
        let ours = schemas.iter().find(|schema| schema.name == keyspace).expect("the keyspace is listed");
        let names: Vec<_> = ours.relations.iter().map(|relation| relation.name.as_str()).collect();
        assert_eq!(names, ["teams", "users"]);
        assert!(session.databases().await.unwrap().contains(&keyspace));

        let columns = session.columns(users(&keyspace)).await.unwrap();
        let summary: Vec<_> = columns.iter().map(|column| (column.name.as_str(), column.column_type.as_str(), column.is_numeric)).collect();
        assert_eq!(
            summary[..4],
            [("team", "text", false), ("id", "int", true), ("active", "boolean", false), ("avatar", "blob", false)]
        );
        assert_eq!(session.primary_key(users(&keyspace)).await.unwrap(), ["team", "id"]);
        assert!(session.foreign_keys(users(&keyspace)).await.unwrap().is_empty());
        assert_eq!(session.estimated_row_count(users(&keyspace)).await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn pages_filters_and_counts() {
    use tableport_core::{FilterOperator, FilterTarget, RowFilter, RowSort};
    in_keyspace("pages", |session, keyspace| async move {
        let columns = session.columns(users(&keyspace)).await.unwrap();
        let index = |name: &str| columns.iter().position(|column| column.name == name).unwrap();
        let all = session.rows(users(&keyspace), columns.clone(), Vec::new(), None, Vec::new(), 0, 100).await.unwrap();
        assert_eq!(all.rows.len(), 5);
        let ada = all.rows.iter().find(|row| row[index("name")].as_deref() == Some("Ada")).unwrap();
        assert_eq!(ada[index("score")].as_deref(), Some("9.50"));
        assert_eq!(ada[index("joined")].as_deref(), Some("2024-01-02"));
        assert_eq!(ada[index("seen")].as_deref(), Some("2024-01-02 03:04:05Z"));
        assert_eq!(ada[index("active")].as_deref(), Some("true"));
        assert_eq!(ada[index("avatar")].as_deref(), Some("\\xcafe"));
        assert_eq!(ada[index("tags")].as_deref(), Some("{'a','b'}"));
        assert_eq!(ada[index("prefs")].as_deref(), Some("{'x':1}"));
        assert_eq!(ada[index("code")].as_deref(), Some("5b6962dd-3f90-4c93-8f61-eabfa4a803e2"));

        // Pages follow on without overlap.
        let first = session.rows(users(&keyspace), columns.clone(), Vec::new(), None, Vec::new(), 0, 2).await.unwrap();
        let second = session.rows(users(&keyspace), columns.clone(), Vec::new(), None, Vec::new(), 2, 2).await.unwrap();
        let third = session.rows(users(&keyspace), columns.clone(), Vec::new(), None, Vec::new(), 4, 2).await.unwrap();
        assert_eq!((first.rows.len(), second.rows.len(), third.rows.len()), (2, 2, 1));
        let mut seen: Vec<_> = first.rows.iter().chain(&second.rows).chain(&third.rows).map(|row| row[index("id")].clone()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5);

        let filter = |target, operator, value: &str| RowFilter { target, operator, value: value.into() };
        let column = |name: &str| FilterTarget::Column { name: name.into() };
        let count = |filters: Vec<RowFilter>| {
            let session = session.clone();
            let relation = users(&keyspace);
            let columns = columns.clone();
            async move { session.row_count(relation, columns, filters).await.unwrap() }
        };
        assert_eq!(count(vec![]).await, 5);
        assert_eq!(count(vec![filter(column("team"), FilterOperator::Equals, "apps")]).await, 3, "on the server");
        assert_eq!(count(vec![filter(column("id"), FilterOperator::GreaterThan, "2")]).await, 3, "ALLOW FILTERING");
        assert_eq!(count(vec![filter(column("name"), FilterOperator::Contains, "A")]).await, 3, "here, no case");
        assert_eq!(count(vec![filter(column("score"), FilterOperator::NotEquals, "7")]).await, 4, "null differs from 7");
        assert_eq!(count(vec![filter(column("score"), FilterOperator::IsNull, "")]).await, 2);
        assert_eq!(count(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "sure")]).await, 1);
        assert_eq!(count(vec![filter(FilterTarget::RawSql, FilterOperator::Equals, "team = 'core'")]).await, 2);
        assert_eq!(count(vec![filter(column("name"), FilterOperator::Equals, "Zoë ✓")]).await, 1);
        assert_eq!(count(vec![filter(column("name"), FilterOperator::Equals, "x' OR 1=1")]).await, 0);
        assert!(session.row_count(users(&keyspace), columns.clone(), vec![filter(column("id"), FilterOperator::Equals, "1; DROP")]).await.is_err());
        let filtered = session
            .rows(users(&keyspace), columns.clone(), vec![filter(column("team"), FilterOperator::Equals, "apps")], None, Vec::new(), 1, 10)
            .await
            .unwrap();
        assert_eq!(filtered.rows.len(), 2);
        let sort = Some(RowSort { column: "name".into(), ascending: true });
        assert!(session.rows(users(&keyspace), columns, Vec::new(), sort, Vec::new(), 0, 10).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn commits_after_checking_the_rows() {
    use tableport_core::{changes_sql, CellDraft, NewRow, PendingChanges, RowUpdate, SqlFunction};
    in_keyspace("commit", |session, keyspace| async move {
        let relation = users(&keyspace);
        let columns = session.columns(relation.clone()).await.unwrap();
        let key = session.primary_key(relation.clone()).await.unwrap();
        let value = |text: &str| CellDraft::Value { text: text.into() };
        let name_of = |id: i32| {
            let session = session.clone();
            let keyspace = keyspace.clone();
            async move {
                let result = session.run_query(format!("SELECT name FROM {keyspace}.users WHERE id = {id} ALLOW FILTERING"), 1).await.unwrap();
                result.result_sets.first().and_then(|set| set.rows.first()).and_then(|row| row[0].clone())
            }
        };

        let changes = PendingChanges {
            updates: vec![RowUpdate {
                key: vec!["core".into(), "2".into()],
                values: HashMap::from([("name".into(), value("Grace's")), ("tags".into(), value("{'x', 'y'}")), ("seen".into(), CellDraft::Function { function: SqlFunction::Now })]),
            }],
            deletes: vec![vec!["apps".into(), "3".into()]],
            inserts: vec![NewRow { values: HashMap::from([("team".into(), value("apps")), ("id".into(), value("6")), ("name".into(), value("new ✓"))]) }],
        };
        let review = changes_sql(&changes, &relation, &columns, &key, Engine::Cassandra).unwrap();
        assert!(review[0].contains("\"name\" = 'Grace''s'") && review[0].contains("WHERE \"team\" = 'core' AND \"id\" = 2"), "{review:?}");
        session.commit(changes, relation.clone(), columns.clone(), key.clone()).await.unwrap();
        assert_eq!(name_of(2).await.as_deref(), Some("Grace's"));
        assert_eq!(name_of(3).await, None);
        assert_eq!(name_of(6).await.as_deref(), Some("new ✓"));

        // A row someone else deleted: nothing is saved.
        let stale = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["core".into(), "1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["apps".into(), "3".into()], values: HashMap::from([("name".into(), value("gone"))]) },
            ],
            ..Default::default()
        };
        let error = session.commit(stale, relation.clone(), columns.clone(), key.clone()).await.unwrap_err();
        assert!(error.message().contains("someone else"), "{error}");
        assert_eq!(name_of(1).await.as_deref(), Some("Ada"));
        // A new row whose key is taken would overwrite it silently: refused.
        let taken = PendingChanges {
            inserts: vec![NewRow { values: HashMap::from([("team".into(), value("core")), ("id".into(), value("1"))]) }],
            ..Default::default()
        };
        assert!(session.commit(taken, relation.clone(), columns.clone(), key.clone()).await.unwrap_err().message().contains("already exists"));
        // Key columns can't change; values are checked against their type.
        let rekey = PendingChanges {
            updates: vec![RowUpdate { key: vec!["core".into(), "1".into()], values: HashMap::from([("team".into(), value("apps"))]) }],
            ..Default::default()
        };
        assert!(session.commit(rekey, relation.clone(), columns.clone(), key.clone()).await.is_err());
        let wrong = PendingChanges {
            updates: vec![RowUpdate { key: vec!["core".into(), "1".into()], values: HashMap::from([("score".into(), value("1; DROP KEYSPACE x"))]) }],
            ..Default::default()
        };
        assert!(session.commit(wrong, relation.clone(), columns.clone(), key.clone()).await.is_err());

        // A read-only connection refuses every change.
        let read_only = DatabaseSession::connect(ConnectionSettings { read_only: true, ..session.settings() }).await.unwrap();
        let delete = PendingChanges { deletes: vec![vec!["core".into(), "1".into()]], ..Default::default() };
        assert!(read_only.commit(delete, relation.clone(), columns, key).await.is_err());
        assert!(read_only.run_query(format!("DELETE FROM {keyspace}.users WHERE team = 'core'"), 1).await.is_err());
        assert!(read_only.run_query(format!("SELECT count(*) FROM {keyspace}.users"), 1).await.is_ok());
        assert_eq!(name_of(1).await.as_deref(), Some("Ada"));

        let full = session.full_rows(relation, session.columns(users(&keyspace)).await.unwrap(), vec!["team".into(), "id".into()], vec![vec!["core".into(), "1".into()]]).await.unwrap();
        assert_eq!(full.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn runs_cql_and_cancels() {
    in_keyspace("query", |session, keyspace| async move {
        let result = session
            .run_query(format!("SELECT name, score FROM {keyspace}.users WHERE team = 'core'; SELECT id FROM users WHERE team = 'apps'"), 100)
            .await
            .unwrap();
        assert_eq!(result.result_sets.len(), 2);
        assert_eq!(result.result_sets[0].numeric_columns, [false, true]);
        assert_eq!(result.result_sets[1].total_rows, 3);
        assert_eq!(result.statement_verbs, ["SELECT", "SELECT"]);

        let result = session.run_query(format!("UPDATE {keyspace}.users SET score = 2 WHERE team = 'apps' AND id = 3; SELECT id FROM users"), 2).await.unwrap();
        assert_eq!(result.result_sets[0].rows.len(), 2);
        assert_eq!(result.result_sets[0].total_rows, 5);
        assert!(session.run_query("SELECT * FROM nope".into(), 1).await.unwrap_err().message().contains("nope"));

        // Cancel drops a long read; the session keeps working.
        for index in 0..300 {
            session.execute(format!("INSERT INTO {keyspace}.teams (id, name) VALUES ({}, 'team {index}')", index + 10)).await.unwrap();
        }
        let running = {
            let session = session.clone();
            let keyspace = keyspace.clone();
            tokio::spawn(async move { session.run_query(format!("SELECT * FROM {keyspace}.teams; SELECT * FROM {keyspace}.teams; SELECT * FROM {keyspace}.teams"), 1).await })
        };
        session.cancel_query().await.unwrap();
        let outcome = running.await.unwrap();
        // A read this small may finish before Cancel arrives; either way the session works.
        if let Err(error) = outcome {
            assert!(error.message().contains("canceled"), "{error}");
        }
        assert!(session.run_query("SELECT release_version FROM system.local".into(), 1).await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn exports_and_changes_structure() {
    use tableport_core::{create_table_sql, drop_relation_sql, truncate_sql, ColumnSpec, ExportFormat, ExportScope, NewTableColumn, StructureChange};
    in_keyspace("structure", |session, keyspace| async move {
        let relation = users(&keyspace);
        let columns = session.columns(relation.clone()).await.unwrap();
        let path = |format: &str| std::env::temp_dir().join(format!("tableport-cassandra-{keyspace}.{format}")).to_string_lossy().into_owned();
        let export = |format, scope| {
            let session = session.clone();
            let relation = relation.clone();
            let columns = columns.clone();
            let file = path(match format {
                ExportFormat::Csv => "csv",
                ExportFormat::Json => "json",
                ExportFormat::Sql => "cql",
            });
            async move { session.export_rows(relation, columns, Vec::new(), None, vec!["team".into(), "id".into()], scope, format, file).await.unwrap() }
        };
        assert_eq!(export(ExportFormat::Csv, ExportScope::All).await, 5);
        assert!(std::fs::read_to_string(path("csv")).unwrap().starts_with("team,id,active,avatar,"));
        assert_eq!(export(ExportFormat::Json, ExportScope::Keys { keys: vec![vec!["core".into(), "1".into()]] }).await, 1);
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path("json")).unwrap()).unwrap();
        assert_eq!(json[0]["score"].to_string(), "9.50");
        assert_eq!(json[0]["active"], serde_json::json!(true));
        // The CQL file loads back.
        assert_eq!(export(ExportFormat::Sql, ExportScope::All).await, 5);
        let cql = std::fs::read_to_string(path("cql")).unwrap();
        session.execute(format!("TRUNCATE {keyspace}.users")).await.unwrap();
        session.run_query(cql, 1).await.unwrap();
        assert_eq!(session.row_count(relation.clone(), columns.clone(), Vec::new()).await.unwrap(), 5);
        for format in ["csv", "json", "cql"] {
            std::fs::remove_file(path(format)).ok();
        }

        let structure = session.structure(relation.clone()).await.unwrap();
        let team = structure.columns.iter().find(|column| column.name == "team").unwrap();
        assert_eq!(team.comment.as_deref(), Some("partition key"));
        assert!(structure.indexes[0].is_primary && structure.indexes[0].definition == "PRIMARY KEY ((\"team\"), \"id\")", "{:?}", structure.indexes);

        let apply = |change: StructureChange| {
            let session = session.clone();
            let relation = relation.clone();
            async move { session.apply_structure_change(relation, change).await }
        };
        let note = ColumnSpec { name: "note".into(), column_type: "text".into(), nullable: true, default_value: None, comment: None };
        apply(StructureChange::AddColumn { column: note }).await.unwrap();
        apply(StructureChange::AddIndex { name: "by_name".into(), columns: vec!["name".into()], unique: false }).await.unwrap();
        let structure = session.structure(relation.clone()).await.unwrap();
        assert!(structure.indexes.iter().any(|index| index.name == "by_name" && index.definition.contains("(name)")), "{:?}", structure.indexes);
        apply(StructureChange::DropIndex { name: "by_name".into(), cascade: false }).await.unwrap();
        apply(StructureChange::DropColumn { name: "note".into(), cascade: false }).await.unwrap();
        let id = structure.columns.iter().find(|column| column.name == "id").unwrap().clone();
        let renamed = ColumnSpec { name: "number".into(), column_type: id.column_type.clone(), nullable: false, default_value: None, comment: None };
        apply(StructureChange::EditColumn { original: id, column: renamed }).await.unwrap();
        assert!(session.columns(relation.clone()).await.unwrap().iter().any(|column| column.name == "number"));
        let name = structure.columns.iter().find(|column| column.name == "name").unwrap().clone();
        let retyped = ColumnSpec { name: "name".into(), column_type: "int".into(), nullable: true, default_value: None, comment: None };
        assert!(apply(StructureChange::EditColumn { original: name, column: retyped }).await.is_err());

        let column = |name: &str, column_type: &str, key: bool| NewTableColumn {
            column: ColumnSpec { name: name.into(), column_type: column_type.into(), nullable: !key, default_value: None, comment: None },
            primary_key: key,
        };
        let statements = create_table_sql(keyspace.clone(), "events".into(), vec![column("id", "timeuuid", true), column("body", "text", false)], Engine::Cassandra).unwrap();
        assert!(!statements[0].contains("NOT NULL"), "{statements:?}");
        session.create_table(keyspace.clone(), "events".into(), vec![column("id", "timeuuid", true), column("body", "text", false)]).await.unwrap();
        let events = Relation { schema: keyspace.clone(), name: "events".into(), kind: RelationKind::Table };
        session.execute(truncate_sql(events.clone(), false, false, Engine::Cassandra)).await.unwrap();
        session.execute(drop_relation_sql(events, false, Engine::Cassandra)).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn ai_access_reads_one_select() {
    use serde_json::{json, Value};
    use tableport_core::{McpConnection, McpHost, McpLogEntry};
    struct Host {
        settings: ConnectionSettings,
    }
    impl McpHost for Host {
        fn connections(&self) -> Vec<McpConnection> {
            vec![McpConnection { id: "c".into(), name: "Cluster".into(), database: self.settings.database.clone(), all_databases: false, tag: None, engine: Engine::Cassandra }]
        }
        fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
            Some(self.settings.clone())
        }
        fn record(&self, _entry: McpLogEntry) {}
    }
    in_keyspace("ai", |session, keyspace| async move {
        let socket = std::env::temp_dir().join(format!("tableport-mcp-cassandra-{}.sock", std::process::id()));
        let mcp = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), Arc::new(Host { settings: session.settings() })).unwrap();
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
        let (text, error) = call!("run_read_query", json!({ "sql": format!("SELECT name, score FROM {keyspace}.users WHERE team = 'core';") }));
        assert!(!error && text.contains("\"Ada\"") && text.contains("9.50"), "{text}");
        let (text, error) = call!("run_read_query", json!({ "sql": format!("SELECT id FROM {keyspace}.users"), "max_rows": 2 }));
        assert!(!error && text.contains("\"truncated\": true"), "{text}");
        let (text, error) = call!("list_tables", json!({}));
        assert!(!error && text.contains("users"), "{text}");
        for cql in [
            format!("DELETE FROM {keyspace}.users WHERE team = 'core'"),
            format!("SELECT * FROM {keyspace}.users; TRUNCATE {keyspace}.users"),
            format!("INSERT INTO {keyspace}.teams (id) VALUES (9)"),
            format!("DROP KEYSPACE {keyspace}"),
            "CREATE ROLE intruder WITH PASSWORD = 'x' AND LOGIN = true".into(),
        ] {
            let (text, error) = call!("run_read_query", json!({ "sql": cql }));
            assert!(error, "not refused: {cql} → {text}");
        }
        mcp.stop();
        assert_eq!(session.row_count(users(&keyspace), session.columns(users(&keyspace)).await.unwrap(), Vec::new()).await.unwrap(), 5);
    })
    .await;
}

//! MySQL and MariaDB tests need servers with a `tableport` user (password
//! `tableport`) that may create databases. List them as
//! `TABLEPORT_TEST_MYSQL=127.0.0.1:3307,127.0.0.1:3308 cargo test`; without
//! it the tests pass without running. Each test runs on every server listed,
//! in a database of its own.

use std::sync::Arc;

use tableport_core::{ConnectionSettings, DatabaseSession, Engine, Relation, RelationKind, TlsMode};

fn servers() -> Vec<(String, u16)> {
    std::env::var("TABLEPORT_TEST_MYSQL")
        .unwrap_or_default()
        .split(',')
        .filter_map(|server| {
            let (host, port) = server.trim().rsplit_once(':')?;
            Some((host.to_string(), port.parse().ok()?))
        })
        .collect()
}

fn settings(host: &str, port: u16, database: &str) -> ConnectionSettings {
    ConnectionSettings {
        host: host.into(),
        port,
        database: database.into(),
        user: "tableport".into(),
        password: Some("tableport".into()),
        tls: TlsMode::Prefer,
        ssh: None,
        read_only: false,
        engine: Engine::Mysql,
    }
}

const SHOP: &[&str] = &[
    "CREATE TABLE teams (id INT PRIMARY KEY, name VARCHAR(40) NOT NULL)",
    "CREATE TABLE users (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(80), score DECIMAL(6,2), joined DATE, \
     seen DATETIME, size ENUM('small','it''s big'), avatar BLOB, team_id INT, FOREIGN KEY (team_id) REFERENCES teams (id))",
    "CREATE VIEW named AS SELECT name FROM users",
    "INSERT INTO teams VALUES (1, 'Core'), (2, 'Apps')",
    "INSERT INTO users (name, score, joined, seen, size, avatar, team_id) VALUES \
     ('Ada', 9.5, '2024-01-02', '2024-01-02 03:04:05', 'small', x'cafe', 1), ('Grace', 7, NULL, NULL, 'it''s big', NULL, 2), \
     ('alan', NULL, NULL, NULL, NULL, NULL, NULL), ('100% sure', 1, NULL, NULL, NULL, NULL, 1), ('back\\slash', NULL, NULL, NULL, NULL, NULL, NULL)",
];

/// Runs `body` on every listed server, in a fresh database that is dropped afterwards.
async fn on_each_server<F, Fut>(name: &str, body: F)
where
    F: Fn(Arc<DatabaseSession>, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for (host, port) in servers() {
        let database = format!("tableport_{name}_{}", std::process::id());
        let admin = DatabaseSession::connect(settings(&host, port, "")).await.expect("connect");
        admin.execute(format!("DROP DATABASE IF EXISTS \"{database}\"")).await.unwrap();
        admin.execute(format!("CREATE DATABASE \"{database}\"")).await.unwrap();
        let session = DatabaseSession::connect(settings(&host, port, &database)).await.expect("connect to the test database");
        for statement in SHOP {
            session.execute(statement.to_string()).await.unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        body(session, database.clone()).await;
        admin.execute(format!("DROP DATABASE \"{database}\"")).await.unwrap();
    }
}

fn users(database: &str) -> Relation {
    Relation { schema: database.into(), name: "users".into(), kind: RelationKind::Table }
}

#[tokio::test]
async fn connects_and_names_the_server() {
    for (host, port) in servers() {
        let session = DatabaseSession::connect(settings(&host, port, "")).await.unwrap();
        let version = session.server_version();
        assert!(version.starts_with("MySQL 8.") || version.starts_with("MariaDB 1"), "{version}");
        session.ping().await.unwrap();
        assert!(!session.is_closed());
        let wrong = ConnectionSettings { password: Some("wrong".into()), ..settings(&host, port, "") };
        let error = tableport_core::test_connection(wrong).await.unwrap_err();
        assert!(error.message().contains("Access denied"), "{error}");
    }
}

#[tokio::test]
async fn browses_databases_columns_and_keys() {
    on_each_server("browse", |session, database| async move {
        let schemas = session.schemas().await.unwrap();
        assert!(schemas.iter().all(|schema| !["mysql", "sys", "information_schema", "performance_schema"].contains(&schema.name.as_str())));
        let ours = schemas.iter().find(|schema| schema.name == database).expect("the test database is listed");
        let names: Vec<_> = ours.relations.iter().map(|relation| (relation.name.as_str(), relation.kind)).collect();
        assert_eq!(names, [("named", RelationKind::View), ("teams", RelationKind::Table), ("users", RelationKind::Table)]);

        let columns = session.columns(users(&database)).await.unwrap();
        let summary: Vec<_> = columns.iter().map(|column| (column.name.as_str(), column.is_numeric, column.kind)).collect();
        use tableport_core::ColumnKind::*;
        assert_eq!(
            summary,
            [
                ("id", true, Other),
                ("name", false, Other),
                ("score", true, Other),
                ("joined", false, Date),
                ("seen", false, Timestamp),
                ("size", false, Enum),
                ("avatar", false, Other),
                ("team_id", true, Other)
            ]
        );
        assert_eq!(columns[5].enum_values, ["small", "it's big"]);
        assert_eq!(session.primary_key(users(&database)).await.unwrap(), ["id"]);
        let keys = session.foreign_keys(users(&database)).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!((keys[0].column.as_str(), keys[0].referenced_relation.name.as_str(), keys[0].referenced_column.as_str()), ("team_id", "teams", "id"));
    })
    .await;
}

#[tokio::test]
async fn pages_filters_and_counts() {
    use tableport_core::{FilterOperator, FilterTarget, RowFilter, RowSort};
    on_each_server("pages", |session, database| async move {
        let columns = session.columns(users(&database)).await.unwrap();
        let key = vec!["id".to_string()];
        let page = session.rows(users(&database), columns.clone(), Vec::new(), None, key.clone(), 0, 2).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0][1].as_deref(), Some("Ada"));
        assert_eq!(page.rows[0][2].as_deref(), Some("9.50"));
        assert_eq!(page.rows[0][4].as_deref(), Some("2024-01-02 03:04:05"));
        assert_eq!(page.rows[0][6].as_deref(), Some("\\xcafe"), "blobs show as hex");
        assert_eq!(page.rows[1][5].as_deref(), Some("it's big"));

        let filter = |target, operator, value: &str| RowFilter { target, operator, value: value.into() };
        let name = || FilterTarget::Column { name: "name".into() };
        let count = |filters: Vec<RowFilter>| {
            let session = session.clone();
            let relation = users(&database);
            let columns = columns.clone();
            async move { session.row_count(relation, columns, filters).await.unwrap() }
        };
        assert_eq!(count(vec![]).await, 5);
        assert_eq!(count(vec![filter(name(), FilterOperator::Contains, "A")]).await, 4, "Ada, Grace, alan, back\\slash: no case");
        assert_eq!(count(vec![filter(name(), FilterOperator::Contains, "%")]).await, 1, "% is literal");
        assert_eq!(count(vec![filter(name(), FilterOperator::Equals, "back\\slash")]).await, 1, "a backslash is literal");
        assert_eq!(count(vec![filter(name(), FilterOperator::Contains, "k\\s")]).await, 1);
        assert_eq!(count(vec![filter(FilterTarget::Column { name: "score".into() }, FilterOperator::NotEquals, "7")]).await, 4, "NULL differs from 7");
        assert_eq!(count(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "cor")]).await, 0);
        assert_eq!(count(vec![filter(FilterTarget::RawSql, FilterOperator::Equals, "team_id = 1")]).await, 2);
        // A value that tries to end its string early stays a value.
        assert_eq!(count(vec![filter(name(), FilterOperator::Equals, "x\\' OR 1=1 -- ")]).await, 0);

        let sorted = session
            .rows(users(&database), columns.clone(), Vec::new(), Some(RowSort { column: "name".into(), ascending: false }), key, 0, 1)
            .await
            .unwrap();
        assert_eq!(sorted.rows[0][1].as_deref(), Some("Grace"));
        assert!(session.estimated_row_count(users(&database)).await.unwrap().is_some());
    })
    .await;
}

#[tokio::test]
async fn commits_edits_inserts_and_deletes_all_or_nothing() {
    use std::collections::HashMap;
    use tableport_core::{CellDraft, NewRow, PendingChanges, RowUpdate};
    on_each_server("commit", |session, database| async move {
        let relation = users(&database);
        let columns = session.columns(relation.clone()).await.unwrap();
        let key = vec!["id".to_string()];
        let value = |text: &str| CellDraft::Value { text: text.into() };
        let names = |session: Arc<DatabaseSession>, relation: Relation| async move {
            let result = session.run_query(format!("SELECT name FROM `{}`.`users` ORDER BY id", relation.schema), 100).await.unwrap();
            result.result_sets[0].rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>()
        };

        // An update that sets a value to itself still counts as found.
        let same = PendingChanges {
            updates: vec![RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("Ada"))]) }],
            ..Default::default()
        };
        session.commit(same, relation.clone(), columns.clone(), key.clone()).await.unwrap();

        let changes = PendingChanges {
            inserts: vec![NewRow { values: HashMap::from([("name".into(), value("it's \\ new"))]) }, NewRow::default()],
            updates: vec![RowUpdate { key: vec!["2".into()], values: HashMap::from([("name".into(), value("Grace H."))]) }],
            deletes: vec![vec!["3".into()]],
        };
        session.commit(changes, relation.clone(), columns.clone(), key.clone()).await.unwrap();
        let after = names(session.clone(), relation.clone()).await;
        assert_eq!(
            after,
            [Some("Ada".into()), Some("Grace H.".into()), Some("100% sure".into()), Some("back\\slash".into()), Some("it's \\ new".into()), None]
        );

        // One missing row: nothing is saved.
        let stale = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["99".into()], values: HashMap::from([("name".into(), value("gone"))]) },
            ],
            ..Default::default()
        };
        let error = session.commit(stale, relation.clone(), columns.clone(), key.clone()).await.unwrap_err();
        assert!(error.message().contains("someone else"), "{error}");
        assert_eq!(names(session.clone(), relation.clone()).await, after);

        // A server error rolls back the statements before it.
        let broken = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["2".into()], values: HashMap::from([("team_id".into(), value("42"))]) },
            ],
            ..Default::default()
        };
        assert!(session.commit(broken, relation.clone(), columns, key).await.is_err());
        assert_eq!(names(session, relation).await, after);
    })
    .await;
}

#[tokio::test]
async fn runs_queries_in_the_servers_own_mode() {
    on_each_server("query", |session, database| async move {
        let users = format!("`{database}`.users");
        // MySQL's quoting in the editor: "…" is a string, \' escapes a quote.
        let result = session
            .run_query(format!("SELECT \"double\", 'it\\'s', id, score, avatar FROM {users} WHERE id = 1; SELECT 1 + 1"), 100)
            .await
            .unwrap();
        assert_eq!(result.result_sets.len(), 2);
        let first = &result.result_sets[0];
        assert_eq!(first.rows[0][0].as_deref(), Some("double"));
        assert_eq!(first.rows[0][1].as_deref(), Some("it's"));
        assert_eq!(first.rows[0][4].as_deref(), Some("\\xcafe"));
        assert_eq!(first.numeric_columns, [false, false, true, true, false]);
        assert_eq!(result.result_sets[1].rows[0][0].as_deref(), Some("2"));

        // Counts for statements without rows, and the row limit.
        let result = session.run_query(format!("UPDATE {users} SET score = 1 WHERE score IS NULL; SELECT name FROM {users}"), 2).await.unwrap();
        assert_eq!(result.statement_counts, [2, 5]);
        assert_eq!(result.result_sets[0].rows.len(), 2);
        assert_eq!(result.result_sets[0].total_rows, 5);
        assert_eq!(result.statement_verbs, ["UPDATE", "SELECT"]);

        // A failing statement rolls back the ones before it.
        let error = session.run_query(format!("UPDATE {users} SET name = 'x'; SELECT * FROM nope"), 10).await.unwrap_err();
        assert!(error.message().contains("nope"), "{error}");
        let names = session.run_query(format!("SELECT count(*) FROM {users} WHERE name = 'x'"), 1).await.unwrap();
        assert_eq!(names.result_sets[0].rows[0][0].as_deref(), Some("0"));

        // Cancel stops a long statement; the session keeps working.
        let running = {
            let session = session.clone();
            tokio::spawn(async move { session.run_query("SELECT SLEEP(30)".into(), 1).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        session.cancel_query().await.unwrap();
        let started = std::time::Instant::now();
        let outcome = running.await.unwrap();
        assert!(started.elapsed().as_secs() < 5, "cancelled quickly");
        // MySQL ends SLEEP with 1 instead of an error; MariaDB with an error.
        if let Ok(result) = outcome {
            assert_eq!(result.result_sets[0].rows[0][0].as_deref(), Some("1"));
        }
        assert!(session.run_query("SELECT 1".into(), 1).await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn exports_and_reads_full_values() {
    use tableport_core::{ExportFormat, ExportScope};
    on_each_server("export", |session, database| async move {
        let relation = users(&database);
        let columns = session.columns(relation.clone()).await.unwrap();
        let key = vec!["id".to_string()];
        let path = |format: &str| std::env::temp_dir().join(format!("tableport-mysql-{database}.{format}")).to_string_lossy().into_owned();
        let export = |format, file: String| {
            let session = session.clone();
            let relation = relation.clone();
            let columns = columns.clone();
            let key = key.clone();
            async move { session.export_rows(relation, columns, Vec::new(), None, key, ExportScope::All, format, file).await.unwrap() }
        };

        assert_eq!(export(ExportFormat::Csv, path("csv")).await, 5);
        let csv = std::fs::read_to_string(path("csv")).unwrap();
        assert!(csv.starts_with("id,name,score,joined,seen,size,avatar,team_id\n1,Ada,9.50,2024-01-02,2024-01-02 03:04:05,small,\\xcafe,1\n"), "{csv}");

        assert_eq!(export(ExportFormat::Json, path("json")).await, 5);
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path("json")).unwrap()).unwrap();
        assert_eq!(json[0]["score"].to_string(), "9.50", "an exact number, as stored");
        assert_eq!(json[0]["avatar"], serde_json::json!("\\xcafe"));
        assert_eq!(json[1]["size"], serde_json::json!("it's big"));
        assert_eq!(json[4]["name"], serde_json::json!("back\\slash"));

        // The SQL file loads back in the server's default mode, values intact.
        assert_eq!(export(ExportFormat::Sql, path("sql")).await, 5);
        let sql = std::fs::read_to_string(path("sql")).unwrap();
        assert!(sql.contains(&format!("INSERT INTO `{database}`.`users` (`id`, `name`")), "{sql}");
        let before = session.run_query(format!("SELECT * FROM `{database}`.users ORDER BY id"), 100).await.unwrap();
        session.run_query(format!("DELETE FROM `{database}`.users; {sql}"), 1).await.unwrap();
        let after = session.run_query(format!("SELECT * FROM `{database}`.users ORDER BY id"), 100).await.unwrap();
        assert_eq!(before.result_sets[0].rows, after.result_sets[0].rows);
        for format in ["csv", "json", "sql"] {
            std::fs::remove_file(path(format)).ok();
        }

        let full = session.full_rows(relation, columns, key, vec![vec!["5".into()], vec!["1".into()]]).await.unwrap();
        assert_eq!(full[0][1].as_deref(), Some("back\\slash"));
        assert_eq!(full[1][6].as_deref(), Some("\\xcafe"));
    })
    .await;
}

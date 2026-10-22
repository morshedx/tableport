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

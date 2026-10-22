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

#[tokio::test]
async fn reads_and_changes_the_structure() {
    use tableport_core::{create_table_sql, truncate_sql, ColumnSpec, ConstraintKind, NewTableColumn, StructureChange};
    on_each_server("structure", |session, database| async move {
        let relation = users(&database);
        session
            .execute(format!(
                "ALTER TABLE \"{database}\".users ADD COLUMN created TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
                 ADD COLUMN label VARCHAR(20) DEFAULT 'none' COMMENT 'shown name', \
                 ADD COLUMN code VARCHAR(10) COLLATE utf8mb4_bin, ADD CONSTRAINT positive CHECK (score >= 0), \
                 ADD UNIQUE KEY unique_label (code)"
            ))
            .await
            .unwrap();
        let structure = session.structure(relation.clone()).await.unwrap();
        let column = |name: &str| structure.columns.iter().find(|column| column.name == name).unwrap().clone();
        assert_eq!(column("id").default_value.as_deref(), Some("AUTO_INCREMENT"));
        assert!(!column("id").nullable);
        assert_eq!(column("label").default_value.as_deref(), Some("'none'"));
        assert_eq!(column("label").comment.as_deref(), Some("shown name"));
        assert!(column("created").default_value.unwrap().to_uppercase().contains("ON UPDATE CURRENT_TIMESTAMP"));
        assert_eq!(column("code").column_type, "varchar(10) COLLATE utf8mb4_bin");
        assert!(structure.indexes.iter().any(|index| index.is_primary && index.definition == "PRIMARY KEY (`id`)"));
        let kinds: Vec<_> = structure.constraints.iter().map(|constraint| constraint.kind).collect();
        assert!(kinds.contains(&ConstraintKind::PrimaryKey) && kinds.contains(&ConstraintKind::ForeignKey));
        assert!(kinds.contains(&ConstraintKind::Unique) && kinds.contains(&ConstraintKind::Check), "{kinds:?}");

        let spec = |detail: &tableport_core::ColumnDetail| ColumnSpec {
            name: detail.name.clone(),
            column_type: detail.column_type.clone(),
            nullable: detail.nullable,
            default_value: detail.default_value.clone(),
            comment: detail.comment.clone(),
        };
        let apply = |change: StructureChange| {
            let session = session.clone();
            let relation = relation.clone();
            async move { session.apply_structure_change(relation, change).await }
        };
        // Renaming keeps the comment, default, collation and AUTO_INCREMENT.
        for name in ["label", "code", "id", "created"] {
            let original = column(name);
            let renamed = ColumnSpec { name: format!("{name}_2"), ..spec(&original) };
            apply(StructureChange::EditColumn { original: original.clone(), column: renamed }).await.unwrap();
        }
        let after = session.structure(relation.clone()).await.unwrap();
        let changed = |name: &str| after.columns.iter().find(|column| column.name == name).unwrap().clone();
        assert_eq!(changed("label_2").comment.as_deref(), Some("shown name"));
        assert_eq!(changed("label_2").default_value.as_deref(), Some("'none'"));
        assert_eq!(changed("code_2").column_type, "varchar(10) COLLATE utf8mb4_bin");
        assert_eq!(changed("id_2").default_value.as_deref(), Some("AUTO_INCREMENT"));
        assert!(changed("created_2").default_value.unwrap().to_uppercase().contains("ON UPDATE CURRENT_TIMESTAMP"));

        // Type, NULL and default together.
        let label = changed("label_2");
        let edited = ColumnSpec { column_type: "varchar(40)".into(), nullable: false, default_value: Some("'x'".into()), ..spec(&label) };
        apply(StructureChange::EditColumn { original: label, column: edited }).await.unwrap();
        let label = session.structure(relation.clone()).await.unwrap().columns.into_iter().find(|column| column.name == "label_2").unwrap();
        assert_eq!((label.column_type.as_str(), label.nullable, label.default_value.as_deref()), ("varchar(40)", false, Some("'x'")));

        // Add and drop: column, index, foreign key, constraints.
        let column_spec = ColumnSpec { name: "note".into(), column_type: "TEXT".into(), nullable: true, default_value: None, comment: Some("it's".into()) };
        apply(StructureChange::AddColumn { column: column_spec }).await.unwrap();
        apply(StructureChange::AddIndex { name: "by_name".into(), columns: vec!["name".into()], unique: false }).await.unwrap();
        apply(StructureChange::DropIndex { name: "by_name".into(), cascade: false }).await.unwrap();
        let foreign = session.structure(relation.clone()).await.unwrap().constraints.into_iter().find(|c| c.kind == ConstraintKind::ForeignKey).unwrap();
        apply(StructureChange::DropConstraint { name: foreign.name, cascade: false }).await.unwrap();
        let teams = Relation { schema: database.clone(), name: "teams".into(), kind: RelationKind::Table };
        apply(StructureChange::AddForeignKey {
            name: Some("user_team".into()),
            column: "team_id".into(),
            referenced: teams,
            referenced_column: "id".into(),
            on_delete: Some("set null".into()),
        })
        .await
        .unwrap();
        apply(StructureChange::DropConstraint { name: "positive".into(), cascade: false }).await.unwrap();
        apply(StructureChange::DropConstraint { name: "unique_label".into(), cascade: false }).await.unwrap();
        apply(StructureChange::DropColumn { name: "note".into(), cascade: false }).await.unwrap();
        let constraints = session.structure(relation.clone()).await.unwrap().constraints;
        let names: Vec<_> = constraints.iter().map(|c| (c.name.as_str(), c.kind)).collect();
        assert_eq!(names, [("PRIMARY", ConstraintKind::PrimaryKey), ("user_team", ConstraintKind::ForeignKey)]);
        assert!(constraints[1].definition.ends_with("ON DELETE SET NULL"), "{}", constraints[1].definition);

        // New table and truncate.
        let columns = vec![
            NewTableColumn { column: ColumnSpec { name: "id".into(), column_type: "INT".into(), nullable: false, default_value: None, comment: None }, primary_key: true },
            NewTableColumn { column: ColumnSpec { name: "title".into(), column_type: "VARCHAR(80)".into(), nullable: true, default_value: None, comment: Some("headline".into()) }, primary_key: false },
        ];
        assert_eq!(create_table_sql(database.clone(), "posts".into(), columns.clone(), Engine::Mysql).unwrap().len(), 1);
        session.create_table(database.clone(), "posts".into(), columns).await.unwrap();
        let posts = Relation { schema: database.clone(), name: "posts".into(), kind: RelationKind::Table };
        session.execute(format!("INSERT INTO \"{database}\".posts VALUES (1, 'a')")).await.unwrap();
        session.execute(truncate_sql(posts.clone(), true, false, Engine::Mysql)).await.unwrap();
        assert_eq!(session.row_count(posts.clone(), Vec::new(), Vec::new()).await.unwrap(), 0);
        assert_eq!(session.structure(posts).await.unwrap().columns[1].comment.as_deref(), Some("headline"));
    })
    .await;
}

/// One allowed MySQL connection, for the AI access attack test.
struct MysqlHost {
    settings: ConnectionSettings,
}

impl tableport_core::McpHost for MysqlHost {
    fn connections(&self) -> Vec<tableport_core::McpConnection> {
        vec![tableport_core::McpConnection {
            id: "server".into(),
            name: "Server".into(),
            database: self.settings.database.clone(),
            all_databases: false,
            tag: None,
        }]
    }

    fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
        Some(self.settings.clone())
    }

    fn record(&self, _entry: tableport_core::McpLogEntry) {}
}

/// The MySQL attack test: every way to write fails, and the rows are unchanged.
#[tokio::test]
async fn ai_tools_only_read() {
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for (host, port) in servers() {
        let database = format!("tableport_ai_{}_{port}", std::process::id());
        let admin = DatabaseSession::connect(settings(&host, port, "")).await.unwrap();
        admin.execute(format!("DROP DATABASE IF EXISTS \"{database}\"")).await.unwrap();
        admin.execute(format!("CREATE DATABASE \"{database}\"")).await.unwrap();
        let owner = DatabaseSession::connect(settings(&host, port, &database)).await.unwrap();
        for statement in [
            "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)",
            "INSERT INTO notes VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            "CREATE FUNCTION wipe() RETURNS INT DETERMINISTIC MODIFIES SQL DATA BEGIN DELETE FROM notes; RETURN 1; END",
        ] {
            owner.execute(statement.into()).await.unwrap_or_else(|error| panic!("{statement}: {error}"));
        }

        let socket = std::env::temp_dir().join(format!("tableport-mcp-mysql-{}-{port}.sock", std::process::id()));
        let host_settings = settings(&host, port, &database);
        let server = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), Arc::new(MysqlHost { settings: host_settings }))
            .unwrap();
        let (reader, mut writer) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut next_id = 0;
        let mut call = |tool: &'static str, arguments: Value| {
            next_id += 1;
            let message = json!({ "jsonrpc": "2.0", "id": next_id, "method": "tools/call", "params": { "name": tool, "arguments": arguments } });
            async move { message }
        };
        macro_rules! ask {
            ($tool:expr, $arguments:expr) => {{
                let message = call($tool, $arguments).await;
                writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
                let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let result = &reply["result"];
                (result["content"][0]["text"].as_str().unwrap_or_default().to_string(), result["isError"].as_bool().unwrap_or(true))
            }};
        }

        let (text, error) = ask!("run_read_query", json!({ "sql": "SELECT id, body FROM notes ORDER BY id" }));
        assert!(!error, "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["rows"][0], json!({ "id": 1, "body": "a" }));
        assert_eq!(body["row_count"], json!(3));

        for sql in [
            "DELETE FROM notes",
            "UPDATE notes SET body = 'x'",
            "SELECT 1; DELETE FROM notes",
            "SELECT 1) AS x; DELETE FROM notes; SELECT * FROM (SELECT 1",
            "WITH x AS (SELECT 1) DELETE FROM notes",
            "SELECT wipe()",
            "SELECT * FROM notes INTO OUTFILE '/tmp/tableport-ai'",
            "SET SESSION TRANSACTION READ WRITE",
            "CREATE TEMPORARY TABLE t (id INT)",
        ] {
            let (text, error) = ask!("run_read_query", json!({ "sql": sql }));
            assert!(error, "not refused: {sql} → {text}");
        }
        for sql in ["DELETE FROM notes", "ANALYZE DELETE FROM notes"] {
            let (text, error) = ask!("explain_query", json!({ "sql": sql }));
            assert!(error, "not refused: EXPLAIN {sql} → {text}");
        }
        let (text, error) = ask!("explain_query", json!({ "sql": "SELECT * FROM notes WHERE id = 1" }));
        assert!(!error && !text.is_empty(), "{text}");

        // A lock taken with GET_LOCK doesn't outlive the call.
        let (text, error) = ask!("run_read_query", json!({ "sql": "SELECT GET_LOCK('tableport_ai', 0) AS got" }));
        assert!(!error && text.contains("\"got\": 1"), "{text}");
        let free = owner.run_query("SELECT IS_FREE_LOCK('tableport_ai')".into(), 1).await.unwrap();
        assert_eq!(free.result_sets[0].rows[0][0].as_deref(), Some("1"));

        let (text, error) = ask!("describe_table", json!({ "schema": database, "table": "notes" }));
        assert!(!error && text.contains("PRIMARY KEY"), "{text}");

        server.stop();
        let count = owner.run_query("SELECT count(*), group_concat(body ORDER BY id) FROM notes".into(), 1).await.unwrap();
        assert_eq!(count.result_sets[0].rows[0], [Some("3".to_string()), Some("a,b,c".to_string())]);
        admin.execute(format!("DROP DATABASE \"{database}\"")).await.unwrap();
    }
}

#[tokio::test]
async fn autocompletes_with_backticks() {
    use tableport_core::{CompletionKind, KeywordCase};
    on_each_server("complete", |session, database| async move {
        session.execute(format!("CREATE TABLE \"{database}\".\"Order Lines\" (\"Line Id\" INT, qty INT)")).await.unwrap();
        let completer = session.completer().await.unwrap();
        let at = |sql: &str| completer.complete(sql.into(), sql.encode_utf16().count() as u32, false, KeywordCase::Upper).items;
        assert_eq!(at("select * from us")[0].label, "users");
        let lines = at("select * from Ord");
        let item = lines.iter().find(|item| item.label == "Order Lines").expect("the table with a space");
        assert_eq!(item.insert_text, "`Order Lines`");
        let sql = "select o. from `Order Lines` o";
        let columns = completer.complete(sql.into(), 9, false, KeywordCase::Upper).items;
        assert_eq!(columns.iter().map(|item| item.insert_text.as_str()).collect::<Vec<_>>(), ["`Line Id`", "qty"]);
        assert!(at("select date_fo").iter().any(|item| item.label == "date_format" && item.kind == CompletionKind::Function));
    })
    .await;
}

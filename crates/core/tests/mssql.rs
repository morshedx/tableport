//! SQL Server tests need a login that may create schemas and tables in its
//! database. Give its connection string, as the Azure portal shows it, in
//! `TABLEPORT_TEST_MSSQL="Server=…;Initial Catalog=…;User ID=…;Password=…" cargo test`;
//! without it the tests pass without running. Each test works in a schema of
//! its own, dropped afterwards: on Azure a new database is a new resource.

use std::sync::Arc;

use tableport_core::{parse_connection_url, ConnectionSettings, DatabaseSession, Engine, Relation, RelationKind, TlsMode};

fn settings() -> Option<ConnectionSettings> {
    let text = std::env::var("TABLEPORT_TEST_MSSQL").ok()?;
    let parsed = parse_connection_url(&text).expect("a SQL Server connection string");
    Some(ConnectionSettings {
        host: parsed.host?,
        port: parsed.port.unwrap_or(1433),
        database: parsed.database.unwrap_or_default(),
        user: parsed.user.unwrap_or_default(),
        password: parsed.password,
        tls: parsed.tls.unwrap_or(TlsMode::Require),
        ssh: None,
        read_only: false,
        engine: Engine::Mssql,
    })
}

fn shop(schema: &str) -> Vec<String> {
    vec![
        format!("CREATE SCHEMA [{schema}]"),
        format!("CREATE TABLE [{schema}].teams (id INT PRIMARY KEY, name NVARCHAR(40) NOT NULL)"),
        format!(
            "CREATE TABLE [{schema}].users (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(80), score DECIMAL(6,2), joined DATE, \
             seen DATETIME2(0), active BIT, avatar VARBINARY(MAX), team_id INT REFERENCES [{schema}].teams (id))"
        ),
        format!("CREATE VIEW [{schema}].named AS SELECT name FROM [{schema}].users"),
        format!("INSERT [{schema}].teams VALUES (1, N'Core'), (2, N'Apps')"),
        format!(
            "INSERT [{schema}].users (name, score, joined, seen, active, avatar, team_id) VALUES \
             (N'Ada', 9.5, '2024-01-02', '2024-01-02 03:04:05', 1, 0xCAFE, 1), (N'Grace', 7, NULL, NULL, 0, NULL, 2), \
             (N'alan', NULL, NULL, NULL, NULL, NULL, NULL), (N'100% sure', 1, NULL, NULL, NULL, NULL, 1), (N'Zoë ✓', NULL, NULL, NULL, NULL, NULL, NULL)"
        ),
    ]
}

/// Runs `body` in a fresh schema that is dropped afterwards.
async fn in_schema<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<DatabaseSession>, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(settings) = settings() else { return };
    let schema = format!("tableport_{name}_{}", std::process::id());
    let session = DatabaseSession::connect(settings).await.expect("connect");
    drop_schema(&session, &schema).await;
    for statement in shop(&schema) {
        session.execute(statement.clone()).await.unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    // The schema is dropped whether `body` passed or not.
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(session.clone(), schema.clone()))).await;
    drop_schema(&session, &schema).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Drops the schema's views, foreign keys and tables, then the schema.
async fn drop_schema(session: &DatabaseSession, schema: &str) {
    let sql = format!(
        "DECLARE @sql nvarchar(max) = N''; \
         SELECT @sql += N'DROP VIEW ' + QUOTENAME(SCHEMA_NAME(schema_id)) + N'.' + QUOTENAME(name) + N'; ' FROM sys.views WHERE SCHEMA_NAME(schema_id) = N'{schema}'; \
         SELECT @sql += N'ALTER TABLE ' + QUOTENAME(OBJECT_SCHEMA_NAME(parent_object_id)) + N'.' + QUOTENAME(OBJECT_NAME(parent_object_id)) + N' DROP CONSTRAINT ' + QUOTENAME(name) + N'; ' \
           FROM sys.foreign_keys WHERE OBJECT_SCHEMA_NAME(parent_object_id) = N'{schema}'; \
         SELECT @sql += N'DROP TABLE ' + QUOTENAME(SCHEMA_NAME(schema_id)) + N'.' + QUOTENAME(name) + N'; ' FROM sys.tables WHERE SCHEMA_NAME(schema_id) = N'{schema}'; \
         IF SCHEMA_ID(N'{schema}') IS NOT NULL SET @sql += N'DROP SCHEMA ' + QUOTENAME(N'{schema}') + N';'; \
         EXEC sp_executesql @sql;"
    );
    session.execute(sql).await.expect("drop the test schema");
}

fn users(schema: &str) -> Relation {
    Relation { schema: schema.into(), name: "users".into(), kind: RelationKind::Table }
}

#[tokio::test]
async fn connects_and_names_the_server() {
    let Some(settings) = settings() else { return };
    let session = DatabaseSession::connect(settings.clone()).await.unwrap();
    let version = session.server_version();
    assert!(version.starts_with("SQL Server ") || version.starts_with("Azure SQL "), "{version}");
    session.ping().await.unwrap();
    assert!(!session.is_closed());
    let wrong = ConnectionSettings { password: Some("wrong".into()), ..settings };
    let error = tableport_core::test_connection(wrong).await.unwrap_err();
    assert!(error.message().contains("Login failed"), "{error}");
}

#[tokio::test]
async fn browses_schemas_columns_and_keys() {
    in_schema("browse", |session, schema| async move {
        let schemas = session.schemas().await.unwrap();
        assert!(schemas.iter().all(|found| !["sys", "INFORMATION_SCHEMA", "db_owner"].contains(&found.name.as_str())));
        let ours = schemas.iter().find(|found| found.name == schema).expect("the test schema is listed");
        let names: Vec<_> = ours.relations.iter().map(|relation| (relation.name.as_str(), relation.kind)).collect();
        assert_eq!(names, [("named", RelationKind::View), ("teams", RelationKind::Table), ("users", RelationKind::Table)]);
        assert!(!session.databases().await.unwrap().is_empty());

        let columns = session.columns(users(&schema)).await.unwrap();
        let summary: Vec<_> = columns.iter().map(|column| (column.name.as_str(), column.column_type.as_str(), column.is_numeric, column.kind)).collect();
        use tableport_core::ColumnKind::*;
        assert_eq!(
            summary,
            [
                ("id", "int", true, Other),
                ("name", "nvarchar(80)", false, Other),
                ("score", "decimal(6,2)", true, Other),
                ("joined", "date", false, Date),
                ("seen", "datetime2(0)", false, Timestamp),
                ("active", "bit", false, Boolean),
                ("avatar", "varbinary(max)", false, Other),
                ("team_id", "int", true, Other)
            ]
        );
        assert_eq!(session.primary_key(users(&schema)).await.unwrap(), ["id"]);
        let keys = session.foreign_keys(users(&schema)).await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!((keys[0].column.as_str(), keys[0].referenced_relation.name.as_str(), keys[0].referenced_column.as_str()), ("team_id", "teams", "id"));
        assert_eq!(tableport_core::qualified_name(users(&schema), Engine::Mssql), format!("[{schema}].[users]"));
    })
    .await;
}

#[tokio::test]
async fn pages_filters_and_counts() {
    use tableport_core::{FilterOperator, FilterTarget, RowFilter, RowSort};
    in_schema("pages", |session, schema| async move {
        let columns = session.columns(users(&schema)).await.unwrap();
        let key = vec!["id".to_string()];
        let page = session.rows(users(&schema), columns.clone(), Vec::new(), None, key.clone(), 0, 2).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0][1].as_deref(), Some("Ada"));
        assert_eq!(page.rows[0][2].as_deref(), Some("9.50"));
        assert_eq!(page.rows[0][3].as_deref(), Some("2024-01-02"));
        assert_eq!(page.rows[0][4].as_deref(), Some("2024-01-02 03:04:05"));
        assert_eq!(page.rows[0][5].as_deref(), Some("true"));
        assert_eq!(page.rows[0][6].as_deref(), Some("\\xcafe"), "bytes show as hex");
        // A table without a key still pages.
        let teams = Relation { schema: schema.clone(), name: "teams".into(), kind: RelationKind::Table };
        let team_columns = session.columns(teams.clone()).await.unwrap();
        assert_eq!(session.rows(teams, team_columns, Vec::new(), None, Vec::new(), 1, 5).await.unwrap().rows.len(), 1);

        let filter = |target, operator, value: &str| RowFilter { target, operator, value: value.into() };
        let name = || FilterTarget::Column { name: "name".into() };
        let count = |filters: Vec<RowFilter>| {
            let session = session.clone();
            let relation = users(&schema);
            let columns = columns.clone();
            async move { session.row_count(relation, columns, filters).await.unwrap() }
        };
        assert_eq!(count(vec![]).await, 5);
        assert_eq!(count(vec![filter(name(), FilterOperator::Contains, "A")]).await, 3, "Ada, Grace, alan: no case");
        assert_eq!(count(vec![filter(name(), FilterOperator::Contains, "%")]).await, 1, "% is literal");
        assert_eq!(count(vec![filter(name(), FilterOperator::Equals, "Zoë ✓")]).await, 1, "text outside the code page survives");
        assert_eq!(count(vec![filter(FilterTarget::Column { name: "score".into() }, FilterOperator::NotEquals, "7")]).await, 4, "NULL differs from 7");
        assert_eq!(count(vec![filter(FilterTarget::Column { name: "active".into() }, FilterOperator::Equals, "true")]).await, 1);
        assert_eq!(count(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "cor")]).await, 0);
        assert_eq!(count(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "sure")]).await, 1);
        assert_eq!(count(vec![filter(FilterTarget::RawSql, FilterOperator::Equals, "team_id = 1")]).await, 2);
        assert_eq!(count(vec![filter(name(), FilterOperator::In, "Ada, alan")]).await, 2);
        assert_eq!(count(vec![filter(name(), FilterOperator::Equals, "x' OR 1=1 -- ")]).await, 0);

        let sorted = session
            .rows(users(&schema), columns.clone(), Vec::new(), Some(RowSort { column: "name".into(), ascending: false }), key, 0, 1)
            .await
            .unwrap();
        assert_eq!(sorted.rows[0][1].as_deref(), Some("Zoë ✓"));
        assert_eq!(session.estimated_row_count(users(&schema)).await.unwrap(), Some(5));
        let view = Relation { schema: schema.clone(), name: "named".into(), kind: RelationKind::View };
        assert_eq!(session.estimated_row_count(view).await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn commits_edits_inserts_and_deletes_all_or_nothing() {
    use std::collections::HashMap;
    use tableport_core::{CellDraft, NewRow, PendingChanges, RowUpdate, SqlFunction};
    in_schema("commit", |session, schema| async move {
        let relation = users(&schema);
        let columns = session.columns(relation.clone()).await.unwrap();
        let key = vec!["id".to_string()];
        let value = |text: &str| CellDraft::Value { text: text.into() };
        let names = |session: Arc<DatabaseSession>, schema: String| async move {
            let result = session.run_query(format!("SELECT name FROM [{schema}].users ORDER BY id"), 100).await.unwrap();
            result.result_sets[0].rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>()
        };

        let same = PendingChanges {
            updates: vec![RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("Ada"))]) }],
            ..Default::default()
        };
        session.commit(same, relation.clone(), columns.clone(), key.clone()).await.unwrap();

        let changes = PendingChanges {
            inserts: vec![
                NewRow { values: HashMap::from([("name".into(), value("it's ✓ new")), ("active".into(), value("false"))]) },
                NewRow { values: HashMap::from([("seen".into(), CellDraft::Function { function: SqlFunction::Now })]) },
            ],
            updates: vec![RowUpdate { key: vec!["2".into()], values: HashMap::from([("name".into(), value("Grace Hopper"))]) }],
            deletes: vec![vec!["3".into()]],
        };
        session.commit(changes, relation.clone(), columns.clone(), key.clone()).await.unwrap();
        let after = names(session.clone(), schema.clone()).await;
        assert_eq!(
            after,
            [Some("Ada".into()), Some("Grace Hopper".into()), Some("100% sure".into()), Some("Zoë ✓".into()), Some("it's ✓ new".into()), None]
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
        assert_eq!(names(session.clone(), schema.clone()).await, after);

        // A server error rolls back the statements before it.
        let broken = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["2".into()], values: HashMap::from([("team_id".into(), value("42"))]) },
            ],
            ..Default::default()
        };
        assert!(session.commit(broken, relation.clone(), columns, key).await.is_err());
        assert_eq!(names(session.clone(), schema.clone()).await, after);

        // A read-only connection refuses every change.
        let read_only = DatabaseSession::connect(ConnectionSettings { read_only: true, ..session.settings() }).await.unwrap();
        let delete = PendingChanges { deletes: vec![vec!["1".into()]], ..Default::default() };
        let columns = read_only.columns(relation.clone()).await.unwrap();
        assert!(read_only.commit(delete, relation.clone(), columns, vec!["id".into()]).await.is_err());
        assert!(read_only.run_query(format!("DELETE FROM [{schema}].users"), 1).await.is_err());
        assert!(read_only.run_query(format!("SELECT COUNT(*) FROM [{schema}].users"), 1).await.is_ok());
        assert_eq!(names(session, schema).await, after);
    })
    .await;
}

#[tokio::test]
async fn runs_scripts_with_batches_counts_and_cancel() {
    in_schema("query", |session, schema| async move {
        let users = format!("[{schema}].users");
        let result = session
            .run_query(format!("SELECT \"name\", N'it''s', id, score, avatar FROM {users} WHERE id = 1\nSELECT 1 + 1 AS two"), 100)
            .await
            .unwrap();
        assert_eq!(result.result_sets.len(), 2);
        let first = &result.result_sets[0];
        assert_eq!(first.rows[0][0].as_deref(), Some("Ada"));
        assert_eq!(first.rows[0][1].as_deref(), Some("it's"));
        assert_eq!(first.rows[0][4].as_deref(), Some("\\xcafe"));
        assert_eq!(first.numeric_columns, [false, false, true, true, false]);
        assert_eq!(result.result_sets[1].rows[0][0].as_deref(), Some("2"));

        // GO splits batches: a view must start its own.
        let script = format!(
            "UPDATE {users} SET score = 1 WHERE score IS NULL;\nDELETE FROM {users} WHERE id = 5;\nGO\n\
             CREATE VIEW [{schema}].scored AS SELECT name FROM {users} WHERE score > 0\ngo\nSELECT name FROM [{schema}].scored"
        );
        let result = session.run_query(script, 2).await.unwrap();
        assert_eq!(result.statement_counts[..2], [2, 1]);
        assert_eq!(result.statement_verbs[..2], ["UPDATE".to_string(), "DELETE".to_string()]);
        assert_eq!(result.result_sets[0].rows.len(), 2);
        assert_eq!(result.result_sets[0].total_rows, 4);

        // A failing statement rolls back the ones before it.
        let error = session.run_query(format!("UPDATE {users} SET name = N'x'\nSELECT * FROM nope"), 10).await.unwrap_err();
        assert!(error.message().contains("nope"), "{error}");
        let names = session.run_query(format!("SELECT COUNT(*) FROM {users} WHERE name = N'x'"), 1).await.unwrap();
        assert_eq!(names.result_sets[0].rows[0][0].as_deref(), Some("0"));

        // Variables live for their batch, as in SSMS.
        let result = session.run_query("DECLARE @n int = 41; SELECT @n + 1".into(), 1).await.unwrap();
        assert_eq!(result.result_sets[0].rows[0][0].as_deref(), Some("42"));

        // Cancel stops a long statement; the session keeps working.
        let running = {
            let session = session.clone();
            tokio::spawn(async move { session.run_query("WAITFOR DELAY '00:00:30'; SELECT 1".into(), 1).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        session.cancel_query().await.unwrap();
        let started = std::time::Instant::now();
        let outcome = running.await.unwrap();
        assert!(started.elapsed().as_secs() < 5, "cancelled quickly");
        assert!(outcome.unwrap_err().message().contains("canceled"));
        assert!(session.run_query("SELECT 1".into(), 1).await.is_ok());
        assert!(!session.is_closed());
    })
    .await;
}

#[tokio::test]
async fn exports_and_reads_full_values() {
    use tableport_core::{ExportFormat, ExportScope};
    in_schema("export", |session, schema| async move {
        let relation = users(&schema);
        let columns = session.columns(relation.clone()).await.unwrap();
        let key = vec!["id".to_string()];
        let path = |format: &str| std::env::temp_dir().join(format!("tableport-mssql-{schema}.{format}")).to_string_lossy().into_owned();
        let export = |format, file: String, scope| {
            let session = session.clone();
            let relation = relation.clone();
            let columns = columns.clone();
            let key = key.clone();
            async move { session.export_rows(relation, columns, Vec::new(), None, key, scope, format, file).await.unwrap() }
        };

        assert_eq!(export(ExportFormat::Csv, path("csv"), ExportScope::All).await, 5);
        let csv = std::fs::read_to_string(path("csv")).unwrap();
        assert!(csv.starts_with("id,name,score,joined,seen,active,avatar,team_id\n1,Ada,9.50,2024-01-02,2024-01-02 03:04:05,true,\\xcafe,1\n"), "{csv}");

        assert_eq!(export(ExportFormat::Json, path("json"), ExportScope::Page { offset: 0, limit: 2 }).await, 2);
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path("json")).unwrap()).unwrap();
        assert_eq!(json[0]["score"].to_string(), "9.50");
        assert_eq!(json[0]["active"], serde_json::json!(true));

        // The SQL file loads back, values intact.
        assert_eq!(export(ExportFormat::Sql, path("sql"), ExportScope::All).await, 5);
        let sql = std::fs::read_to_string(path("sql")).unwrap();
        assert!(sql.contains(&format!("INSERT INTO [{schema}].[users] ([id], [name]")), "{sql}");
        let before = session.run_query(format!("SELECT * FROM [{schema}].users ORDER BY id"), 100).await.unwrap();
        session
            .run_query(format!("DELETE FROM [{schema}].users\nSET IDENTITY_INSERT [{schema}].users ON\n{sql}\nSET IDENTITY_INSERT [{schema}].users OFF"), 1)
            .await
            .unwrap();
        let after = session.run_query(format!("SELECT * FROM [{schema}].users ORDER BY id"), 100).await.unwrap();
        assert_eq!(before.result_sets[0].rows, after.result_sets[0].rows);

        let keys = vec![vec!["2".to_string()], vec!["1".to_string()]];
        assert_eq!(export(ExportFormat::Csv, path("csv"), ExportScope::Keys { keys: keys.clone() }).await, 2);
        for format in ["csv", "json", "sql"] {
            std::fs::remove_file(path(format)).ok();
        }

        let full = session.full_rows(relation, columns, key, vec![vec!["5".into()], vec!["1".into()]]).await.unwrap();
        assert_eq!(full[0][1].as_deref(), Some("Zoë ✓"));
        assert_eq!(full[1][6].as_deref(), Some("\\xcafe"));
    })
    .await;
}

#[tokio::test]
async fn reads_and_changes_the_structure() {
    use tableport_core::{create_table_sql, truncate_sql, ColumnSpec, ConstraintKind, NewTableColumn, StructureChange};
    in_schema("structure", |session, schema| async move {
        let relation = users(&schema);
        session
            .execute(format!(
                "ALTER TABLE [{schema}].users ADD label NVARCHAR(20) CONSTRAINT label_default DEFAULT N'none', \
                 code VARCHAR(10) COLLATE Latin1_General_BIN2, doubled AS (score * 2) PERSISTED, \
                 CONSTRAINT positive CHECK (score >= 0), CONSTRAINT unique_code UNIQUE (code)"
            ))
            .await
            .unwrap();
        let structure = session.structure(relation.clone()).await.unwrap();
        let column = |name: &str| structure.columns.iter().find(|column| column.name == name).unwrap().clone();
        assert_eq!(column("id").default_value.as_deref(), Some("IDENTITY(1,1)"));
        assert_eq!(column("label").default_value.as_deref(), Some("N'none'"));
        assert_eq!(column("code").column_type, "varchar(10) COLLATE Latin1_General_BIN2");
        assert!(column("doubled").default_value.unwrap().starts_with("AS ("));
        assert!(structure.indexes.iter().any(|index| index.is_primary && index.definition.starts_with("PRIMARY KEY CLUSTERED ([id])")));
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
        // Rename, retype, NOT NULL and a new default in one change.
        let label = column("label");
        let edited = ColumnSpec {
            name: "title".into(),
            column_type: "nvarchar(40)".into(),
            nullable: true,
            default_value: Some("N'untitled'".into()),
            ..spec(&label)
        };
        let preview = session.structure_change_statements(relation.clone(), StructureChange::EditColumn { original: label.clone(), column: edited.clone() }).await.unwrap();
        assert!(preview.iter().any(|statement| statement.contains("DROP CONSTRAINT [label_default]")), "{preview:?}");
        apply(StructureChange::EditColumn { original: label, column: edited }).await.unwrap();
        let after = session.structure(relation.clone()).await.unwrap();
        let title = after.columns.iter().find(|column| column.name == "title").unwrap();
        assert_eq!((title.column_type.as_str(), title.default_value.as_deref()), ("nvarchar(40)", Some("N'untitled'")));
        // Identity and computed columns can't be changed in place.
        let id = column("id");
        assert!(apply(StructureChange::EditColumn { original: id.clone(), column: ColumnSpec { column_type: "bigint".into(), ..spec(&id) } }).await.is_err());

        // A failing step leaves the table as it was: the rename isn't kept.
        let score = column("score");
        let failing = ColumnSpec { name: "points".into(), default_value: Some("no_such_function()".into()), ..spec(&score) };
        assert!(apply(StructureChange::EditColumn { original: score, column: failing }).await.is_err());
        assert!(session.columns(relation.clone()).await.unwrap().iter().any(|column| column.name == "score"));

        // Add and drop: column, index, foreign key, constraints.
        let note = ColumnSpec { name: "note".into(), column_type: "nvarchar(max)".into(), nullable: false, default_value: Some("N''".into()), comment: None };
        apply(StructureChange::AddColumn { column: note }).await.unwrap();
        apply(StructureChange::AddIndex { name: "by_name".into(), columns: vec!["name".into()], unique: false }).await.unwrap();
        apply(StructureChange::DropIndex { name: "by_name".into(), cascade: false }).await.unwrap();
        apply(StructureChange::DropIndex { name: "unique_code".into(), cascade: false }).await.unwrap();
        let foreign = session.structure(relation.clone()).await.unwrap().constraints.into_iter().find(|c| c.kind == ConstraintKind::ForeignKey).unwrap();
        apply(StructureChange::DropConstraint { name: foreign.name, cascade: false }).await.unwrap();
        let teams = Relation { schema: schema.clone(), name: "teams".into(), kind: RelationKind::Table };
        let restrict = StructureChange::AddForeignKey {
            name: None,
            column: "team_id".into(),
            referenced: teams.clone(),
            referenced_column: "id".into(),
            on_delete: Some("restrict".into()),
        };
        assert!(apply(restrict).await.unwrap_err().message().contains("NO ACTION"));
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
        // The column's default constraint goes first.
        apply(StructureChange::DropColumn { name: "note".into(), cascade: false }).await.unwrap();
        let structure = session.structure(relation.clone()).await.unwrap();
        assert!(structure.columns.iter().all(|column| column.name != "note"));
        let foreign = structure.constraints.iter().find(|c| c.kind == ConstraintKind::ForeignKey).unwrap();
        assert!(foreign.definition.ends_with("ON DELETE SET NULL"), "{}", foreign.definition);

        // A new table with an IDENTITY key, then emptied.
        let column = |name: &str, column_type: &str, default: Option<&str>, key: bool| NewTableColumn {
            column: ColumnSpec { name: name.into(), column_type: column_type.into(), nullable: !key, default_value: default.map(str::to_string), comment: None },
            primary_key: key,
        };
        let statements = create_table_sql(
            schema.clone(),
            "events".into(),
            vec![column("id", "bigint", Some("IDENTITY(1,1)"), true), column("at", "datetime2", Some("SYSDATETIME()"), false)],
            Engine::Mssql,
        )
        .unwrap();
        assert!(statements[0].contains("\"id\" bigint IDENTITY(1,1)"), "{statements:?}");
        session.create_table(schema.clone(), "events".into(), vec![column("id", "bigint", Some("IDENTITY(1,1)"), true)]).await.unwrap();
        let events = Relation { schema: schema.clone(), name: "events".into(), kind: RelationKind::Table };
        session.execute(truncate_sql(events, true, true, Engine::Mssql)).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn ai_access_reads_and_refuses_writes() {
    use serde_json::{json, Value};
    use tableport_core::{McpConnection, McpHost, McpLogEntry};
    struct Host {
        settings: ConnectionSettings,
    }
    impl McpHost for Host {
        fn connections(&self) -> Vec<McpConnection> {
            vec![McpConnection {
                id: "sql".into(),
                name: "Azure".into(),
                database: self.settings.database.clone(),
                all_databases: false,
                tag: None,
                engine: Engine::Mssql,
            }]
        }
        fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
            Some(self.settings.clone())
        }
        fn record(&self, _entry: McpLogEntry) {}
    }
    in_schema("ai", |session, schema| async move {
        let socket = std::env::temp_dir().join(format!("tableport-mcp-mssql-{}.sock", std::process::id()));
        let host = Arc::new(Host { settings: session.settings() });
        let mcp = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), host).unwrap();
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
        let users = format!("[{schema}].users");
        let (text, error) = call!("run_read_query", json!({ "sql": format!("SELECT TOP 2 name, active FROM {users} ORDER BY name DESC;") }));
        assert!(!error && text.contains("Zoë ✓") && text.contains("\"row_count\": 2"), "{text}");
        let (text, error) = call!("run_read_query", json!({ "sql": format!("WITH t AS (SELECT COUNT(*) AS n FROM {users}) SELECT n FROM t") }));
        assert!(!error && text.contains("\"n\": 5"), "{text}");
        let (text, error) = call!("run_read_query", json!({ "sql": format!("SELECT name FROM {users}"), "max_rows": 2 }));
        assert!(!error && text.contains("\"truncated\": true"), "{text}");
        let (text, error) = call!("explain_query", json!({ "sql": format!("SELECT name FROM {users} WHERE id = 1") }));
        assert!(!error && text.contains("Clustered Index Seek"), "{text}");
        let (text, error) = call!("describe_table", json!({ "table": "users", "schema": schema }));
        assert!(!error && text.contains("decimal(6,2)"), "{text}");
        for sql in [
            format!("DELETE FROM {users}"),
            format!("SELECT 1 DELETE FROM {users}"),
            format!("SELECT * INTO [{schema}].copy FROM {users}"),
            format!("SELECT 1; COMMIT; DELETE FROM {users}"),
            format!("SELECT 1; UPDATE {users} SET name = N'x'"),
            "EXEC sp_who".into(),
            "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'DELETE x')".into(),
            format!("WITH a AS (SELECT 1 AS x) DELETE FROM {users}"),
            format!("SELECT 1; DROP TABLE {users}"),
            "SELECT 1 WAITFOR DELAY '00:01:00'".into(),
        ] {
            let (text, error) = call!("run_read_query", json!({ "sql": sql }));
            assert!(error, "not refused: {sql} → {text}");
        }
        mcp.stop();
        let count = session.run_query(format!("SELECT COUNT(*) FROM {users} WHERE name <> N'x'"), 1).await.unwrap();
        assert_eq!(count.result_sets[0].rows[0][0].as_deref(), Some("5"));
    })
    .await;
}

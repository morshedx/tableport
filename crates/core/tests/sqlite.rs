use tableport_core::{ConnectionSettings, DatabaseSession, Engine, TlsMode};

/// A fresh SQLite file in the temp folder, removed when dropped.
struct TempDatabase(std::path::PathBuf);

impl TempDatabase {
    fn new(name: &str, setup: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tableport-{name}-{}.sqlite", std::process::id()));
        std::fs::remove_file(&path).ok();
        rusqlite::Connection::open(&path).unwrap().execute_batch(setup).unwrap();
        Self(path)
    }

    fn settings(&self, read_only: bool) -> ConnectionSettings {
        ConnectionSettings {
            host: String::new(),
            port: 0,
            database: self.0.to_string_lossy().into(),
            user: String::new(),
            password: None,
            tls: TlsMode::Disable,
            ssh: None,
            read_only,
            engine: Engine::Sqlite,
        }
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

#[tokio::test]
async fn opens_a_file_and_runs_sql() {
    let file = TempDatabase::new("open", "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, score REAL, data BLOB);
         INSERT INTO notes VALUES (1, 'first', 1.5, x'00ff'), (2, NULL, NULL, NULL);");
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    assert!(session.server_version().starts_with("SQLite 3."));
    assert_eq!(session.engine(), Engine::Sqlite);
    session.execute("INSERT INTO notes (id, body) VALUES (3, 'third')".into()).await.unwrap();
    session.ping().await.unwrap();
    assert!(!session.is_closed());
    session.close();
    assert!(session.is_closed());
}

#[tokio::test]
async fn read_only_files_refuse_writes() {
    let file = TempDatabase::new("readonly", "CREATE TABLE notes (id INTEGER PRIMARY KEY);");
    let session = DatabaseSession::connect(file.settings(true)).await.unwrap();
    for sql in [
        "INSERT INTO notes VALUES (1)",
        "PRAGMA query_only = OFF; INSERT INTO notes VALUES (1)",
        "CREATE TABLE other (id INTEGER)",
    ] {
        assert!(session.execute(sql.into()).await.is_err(), "not refused: {sql}");
    }
}

#[tokio::test]
async fn a_missing_file_is_an_error_and_is_not_created() {
    let path = std::env::temp_dir().join(format!("tableport-missing-{}.sqlite", std::process::id()));
    std::fs::remove_file(&path).ok();
    let mut settings = TempDatabase::new("unused", "").settings(false);
    settings.database = path.to_string_lossy().into();
    assert!(DatabaseSession::connect(settings).await.is_err());
    assert!(!path.exists());
}

const SHOP: &str = "
    CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
    CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL, joined DATE, avatar BLOB,
                        team_id INTEGER REFERENCES teams (id));
    CREATE VIEW named AS SELECT name FROM users;
    INSERT INTO teams VALUES (1, 'Core'), (2, 'Apps');
    INSERT INTO users VALUES (1, 'Ada', 9.5, '2024-01-02', x'cafe', 1), (2, 'Grace', 7, NULL, NULL, 2),
                             (3, 'alan', NULL, NULL, NULL, NULL), (4, '100% sure', 1, NULL, NULL, 1);";

fn users() -> tableport_core::Relation {
    tableport_core::Relation { schema: "main".into(), name: "users".into(), kind: tableport_core::RelationKind::Table }
}

#[tokio::test]
async fn browses_schemas_columns_and_keys() {
    let file = TempDatabase::new("browse", SHOP);
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let schemas = session.schemas().await.unwrap();
    assert_eq!(schemas.len(), 1);
    let names: Vec<_> = schemas[0].relations.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(names, [
        ("named", tableport_core::RelationKind::View),
        ("teams", tableport_core::RelationKind::Table),
        ("users", tableport_core::RelationKind::Table),
    ]);

    let columns = session.columns(users()).await.unwrap();
    let summary: Vec<_> = columns.iter().map(|c| (c.name.as_str(), c.is_numeric)).collect();
    assert_eq!(summary, [("id", true), ("name", false), ("score", true), ("joined", false), ("avatar", false), ("team_id", true)]);
    assert_eq!(columns[3].kind, tableport_core::ColumnKind::Date);
    assert_eq!(session.primary_key(users()).await.unwrap(), ["id"]);
    let keys = session.foreign_keys(users()).await.unwrap();
    assert_eq!((keys[0].column.as_str(), keys[0].referenced_relation.name.as_str(), keys[0].referenced_column.as_str()), ("team_id", "teams", "id"));
    assert_eq!(session.databases().await.unwrap(), [file.0.to_string_lossy().to_string()]);
    assert!(session.routines("main".into()).await.unwrap().is_empty());
}

#[tokio::test]
async fn pages_filters_and_counts() {
    use tableport_core::{FilterOperator, FilterTarget, RowFilter, RowSort};
    let file = TempDatabase::new("pages", SHOP);
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let columns = session.columns(users()).await.unwrap();
    let by_id = Some(RowSort { column: "id".into(), ascending: true });
    let page = session.rows(users(), columns.clone(), vec![], by_id.clone(), vec!["id".into()], 0, 2).await.unwrap();
    assert_eq!(page.rows[0], [Some("1".into()), Some("Ada".into()), Some("9.5".into()), Some("2024-01-02".into()), Some("\\xcafe".into()), Some("1".into())]);
    assert_eq!(page.rows.len(), 2);

    let filter = |target, operator, value: &str| RowFilter { target, operator, value: value.into() };
    let name = || FilterTarget::Column { name: "name".into() };
    let cases = [
        (filter(name(), FilterOperator::Contains, "A"), vec!["1", "2", "3"]),
        (filter(name(), FilterOperator::Contains, "100%"), vec!["4"]),
        (filter(name(), FilterOperator::StartsWith, "a"), vec!["1", "3"]),
        (filter(name(), FilterOperator::NotEquals, "Ada"), vec!["2", "3", "4"]),
        (filter(FilterTarget::Column { name: "score".into() }, FilterOperator::GreaterThan, "5"), vec!["1", "2"]),
        (filter(FilterTarget::Column { name: "score".into() }, FilterOperator::IsNull, ""), vec!["3"]),
        (filter(FilterTarget::AnyColumn, FilterOperator::Contains, "grace"), vec!["2"]),
        (filter(FilterTarget::Column { name: "id".into() }, FilterOperator::In, "2, 4"), vec!["2", "4"]),
    ];
    for (filter, expected) in cases {
        let page = session.rows(users(), columns.clone(), vec![filter.clone()], by_id.clone(), vec![], 0, 10).await.unwrap();
        let ids: Vec<_> = page.rows.iter().map(|row| row[0].clone().unwrap_or_default()).collect();
        assert_eq!(ids, expected, "{filter:?}");
        let count = session.row_count(users(), columns.clone(), vec![filter.clone()]).await.unwrap();
        assert_eq!(count as usize, expected.len(), "{filter:?}");
    }
    assert_eq!(session.estimated_row_count(users()).await.unwrap(), Some(4));
}

#[tokio::test]
async fn commits_edits_inserts_and_deletes_all_or_nothing() {
    use std::collections::HashMap;
    use tableport_core::{CellDraft, NewRow, PendingChanges, RowUpdate, SqlFunction};
    let file = TempDatabase::new("commit", SHOP);
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let columns = session.columns(users()).await.unwrap();
    let key = vec!["id".to_string()];
    let value = |text: &str| CellDraft::Value { text: text.into() };

    let changes = PendingChanges {
        updates: vec![RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("Ada L.")), ("score".into(), value("10"))]) }],
        deletes: vec![vec!["3".into()]],
        inserts: vec![NewRow { values: HashMap::from([
            ("name".into(), value("Linus")),
            ("joined".into(), CellDraft::Function { function: SqlFunction::CurrentDate }),
            // SQLite has no DEFAULT keyword: the column is left out.
            ("score".into(), CellDraft::Default),
        ]) }],
    };
    session.commit(changes, users(), columns.clone(), key.clone()).await.unwrap();
    let rows = session.rows(users(), columns.clone(), vec![], None, key.clone(), 0, 10).await.unwrap().rows;
    let names: Vec<_> = rows.iter().map(|row| row[1].clone().unwrap_or_default()).collect();
    assert_eq!(names, ["Ada L.", "Grace", "100% sure", "Linus"]);
    assert_eq!(rows[0][2].as_deref(), Some("10.0"), "REAL affinity stores the typed text as a number");
    assert!(rows[3][3].is_some(), "CURRENT_DATE filled the new row's date");

    // A row deleted meanwhile: nothing is saved, the update before it included.
    session.execute("DELETE FROM users WHERE id = 2".into()).await.unwrap();
    let changes = PendingChanges {
        updates: vec![
            RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
            RowUpdate { key: vec!["2".into()], values: HashMap::from([("name".into(), value("gone"))]) },
        ],
        ..Default::default()
    };
    let error = session.commit(changes, users(), columns.clone(), key.clone()).await.unwrap_err();
    assert!(error.message().contains("someone else"), "{error}");
    let first = session.rows(users(), columns.clone(), vec![], None, key.clone(), 0, 1).await.unwrap().rows;
    assert_eq!(first[0][1].as_deref(), Some("Ada L."));

    // Setting an existing value back to DEFAULT is refused, with a reason.
    let changes = PendingChanges {
        updates: vec![RowUpdate { key: vec!["1".into()], values: HashMap::from([("score".into(), CellDraft::Default)]) }],
        ..Default::default()
    };
    let error = session.commit(changes, users(), columns, key).await.unwrap_err();
    assert!(error.message().contains("default"), "{error}");
}

#[tokio::test]
async fn runs_queries_as_one_transaction() {
    let file = TempDatabase::new("query", SHOP);
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let result = session
        .run_query("UPDATE users SET score = score + 1 WHERE id = 1; SELECT id, name, score, 2 * 3 AS six FROM users ORDER BY id".into(), 2)
        .await
        .unwrap();
    assert_eq!(result.statement_verbs, ["UPDATE", "SELECT"]);
    assert_eq!(result.statement_counts, [1, 4]);
    let set = &result.result_sets[0];
    assert_eq!(set.columns, ["id", "name", "score", "six"]);
    assert_eq!((set.rows.len(), set.total_rows), (2, 4));
    assert_eq!(set.rows[0][2].as_deref(), Some("10.5"));
    assert_eq!(set.numeric_columns, [true, false, true, true]);

    // A failing statement undoes the ones before it.
    let error = session.run_query("DELETE FROM users; SELECT * FROM no_such_table".into(), 10).await.unwrap_err();
    assert!(error.message().contains("no_such_table"), "{error}");
    let count = session.run_query("SELECT count(*) FROM users".into(), 1).await.unwrap();
    assert_eq!(count.result_sets[0].rows[0][0].as_deref(), Some("4"));
}

#[tokio::test]
async fn exports_and_reads_full_values() {
    use tableport_core::{ExportFormat, ExportScope};
    let file = TempDatabase::new("export", SHOP);
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let columns = session.columns(users()).await.unwrap();
    let key = vec!["id".to_string()];
    let out = |format: &str| std::env::temp_dir().join(format!("tableport-export-{}.{format}", std::process::id()));
    for (format, path) in [(ExportFormat::Csv, out("csv")), (ExportFormat::Json, out("json")), (ExportFormat::Sql, out("sql"))] {
        let written = session
            .export_rows(users(), columns.clone(), vec![], None, key.clone(), ExportScope::All, format, path.to_string_lossy().into())
            .await
            .unwrap();
        assert_eq!(written, 4);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        match format {
            ExportFormat::Csv => assert!(text.starts_with("id,name,score,joined,avatar,team_id\n1,Ada,9.5,2024-01-02,\\xcafe,1\n"), "{text}"),
            ExportFormat::Json => {
                let rows: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(rows[0]["score"], serde_json::json!(9.5));
                assert_eq!(rows[2]["score"], serde_json::Value::Null);
            }
            ExportFormat::Sql => assert!(text.contains("VALUES (4, '100% sure', 1.0, NULL, NULL, 1);"), "{text}"),
        }
    }
    let full = session.full_rows(users(), columns, key, vec![vec!["1".into()]]).await.unwrap();
    assert_eq!(full[0][4].as_deref(), Some("\\xcafe"));
}

#[tokio::test]
async fn reads_and_changes_the_structure() {
    use tableport_core::{create_table_sql, drop_relation_sql, structure_change_sql, truncate_sql, ColumnSpec, ConstraintKind, NewTableColumn, StructureChange};
    let file = TempDatabase::new("structure", &(SHOP.to_string() + "CREATE UNIQUE INDEX users_name ON users (name);"));
    let session = DatabaseSession::connect(file.settings(false)).await.unwrap();
    let structure = session.structure(users()).await.unwrap();
    assert_eq!(structure.columns.len(), 6);
    assert!(!structure.columns[0].nullable, "the INTEGER PRIMARY KEY is never NULL");
    assert!(structure.indexes.iter().any(|index| index.name == "users_name" && index.is_unique && index.definition.starts_with("CREATE UNIQUE INDEX")));
    let kinds: Vec<_> = structure.constraints.iter().map(|c| (c.kind, c.definition.as_str())).collect();
    assert!(kinds.contains(&(ConstraintKind::PrimaryKey, "PRIMARY KEY (id)")), "{kinds:?}");
    assert!(kinds.contains(&(ConstraintKind::ForeignKey, "FOREIGN KEY (team_id) REFERENCES teams(id)")), "{kinds:?}");

    let spec = |name: &str, column_type: &str| ColumnSpec { name: name.into(), column_type: column_type.into(), nullable: true, default_value: None, comment: None };
    // In place: add, rename, index, drop.
    session.apply_structure_change(users(), StructureChange::AddColumn { column: spec("email", "TEXT") }).await.unwrap();
    let original = session.structure(users()).await.unwrap().columns.into_iter().find(|c| c.name == "email").unwrap();
    session.apply_structure_change(users(), StructureChange::EditColumn { original: original.clone(), column: spec("mail", "TEXT") }).await.unwrap();
    session.apply_structure_change(users(), StructureChange::AddIndex { name: "users_mail".into(), columns: vec!["mail".into()], unique: false }).await.unwrap();
    assert!(session.structure(users()).await.unwrap().indexes.iter().any(|index| index.name == "users_mail"));
    session.apply_structure_change(users(), StructureChange::DropIndex { name: "users_mail".into(), cascade: false }).await.unwrap();
    session.apply_structure_change(users(), StructureChange::DropColumn { name: "mail".into(), cascade: false }).await.unwrap();
    assert_eq!(session.structure(users()).await.unwrap().columns.len(), 6);

    // Not in place: refused with the reason, nothing changed.
    let renamed = ColumnSpec { column_type: "INTEGER".into(), ..spec("name", "INTEGER") };
    let column = session.structure(users()).await.unwrap().columns[1].clone();
    let error = structure_change_sql(users(), StructureChange::EditColumn { original: column, column: renamed }, Engine::Sqlite).unwrap_err();
    assert!(error.message().contains("type"), "{error}");

    // Truncate and drop are SQLite's own statements.
    assert_eq!(truncate_sql(users(), true, true, Engine::Sqlite), r#"DELETE FROM "main"."users";"#);
    assert_eq!(drop_relation_sql(users(), true, Engine::Sqlite), r#"DROP TABLE "main"."users";"#);

    // New table, with SQLite's own key type.
    let columns = vec![
        NewTableColumn { column: ColumnSpec { nullable: false, ..spec("id", "INTEGER") }, primary_key: true },
        NewTableColumn { column: spec("title", "TEXT"), primary_key: false },
    ];
    assert!(create_table_sql("main".into(), "posts".into(), columns.clone(), Engine::Sqlite).is_ok());
    session.create_table("main".into(), "posts".into(), columns).await.unwrap();
    session.execute("INSERT INTO posts (title) VALUES ('hello')".into()).await.unwrap();
    let rows = session.run_query("SELECT id FROM posts".into(), 1).await.unwrap();
    assert_eq!(rows.result_sets[0].rows[0][0].as_deref(), Some("1"), "INTEGER PRIMARY KEY numbers rows itself");
}

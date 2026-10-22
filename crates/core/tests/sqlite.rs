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

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

//! Cloudflare D1 tests need a D1 database they may create tables in:
//! `TABLEPORT_TEST_D1=<account id>/<database id> TABLEPORT_TEST_D1_TOKEN=<API token> cargo test`.
//! Without them the tests pass without running. Each test makes its own
//! tables and drops them afterwards.

use std::collections::HashMap;
use std::sync::Arc;

use tableport_core::{ConnectionSettings, DatabaseSession, Engine, Relation, RelationKind, TlsMode};

fn settings() -> Option<ConnectionSettings> {
    let target = std::env::var("TABLEPORT_TEST_D1").ok()?;
    let (account, database) = target.split_once('/')?;
    Some(ConnectionSettings {
        host: String::new(),
        port: 0,
        database: database.into(),
        user: account.into(),
        password: std::env::var("TABLEPORT_TEST_D1_TOKEN").ok(),
        tls: TlsMode::VerifyFull,
        ssh: None,
        read_only: false,
        engine: Engine::D1,
    })
}

async fn with_tables<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<DatabaseSession>, Relation) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(settings) = settings() else { return };
    let session = DatabaseSession::connect(settings).await.expect("connect");
    let users = format!("users_{name}_{}", std::process::id());
    let teams = format!("teams_{name}_{}", std::process::id());
    session.execute(format!("DROP TABLE IF EXISTS {users}; DROP TABLE IF EXISTS {teams}")).await.unwrap();
    session
        .execute(format!(
            "CREATE TABLE {teams} (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n\
             CREATE TABLE {users} (id INTEGER PRIMARY KEY, name TEXT, score REAL, avatar BLOB, team_id INTEGER REFERENCES {teams} (id));\n\
             INSERT INTO {teams} VALUES (1, 'Core'), (2, 'Apps');\n\
             INSERT INTO {users} (name, score, avatar, team_id) VALUES ('Ada', 9.5, x'cafe', 1), ('Grace', 7, NULL, 2), ('Zoë ✓', NULL, NULL, NULL)"
        ))
        .await
        .expect("set up the tables");
    let relation = Relation { schema: "main".into(), name: users.clone(), kind: RelationKind::Table };
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(session.clone(), relation))).await;
    session.execute(format!("DROP TABLE IF EXISTS {users}; DROP TABLE IF EXISTS {teams}")).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn a_failing_batch_saves_nothing() {
    with_tables("atomic", |session, users| async move {
        let table = &users.name;
        let error = session.execute(format!("INSERT INTO {table} (name) VALUES ('kept?'); INSERT INTO nope VALUES (1)")).await.unwrap_err();
        assert!(error.message().contains("nope"), "{error}");
        let rows = session.run_query(format!("SELECT count(*) FROM {table} WHERE name = 'kept?'"), 1).await.unwrap();
        assert_eq!(rows.result_sets[0].rows[0][0].as_deref(), Some("0"), "the batch was all or nothing");
    })
    .await;
}

#[tokio::test]
async fn browses_commits_and_rebuilds() {
    use tableport_core::{CellDraft, ColumnSpec, NewRow, PendingChanges, RowUpdate, StructureChange};
    with_tables("browse", |session, users| async move {
        assert!(session.server_version().starts_with("D1 (SQLite 3."), "{}", session.server_version());
        let schemas = session.schemas().await.unwrap();
        assert!(schemas[0].relations.iter().any(|relation| relation.name == users.name));
        assert!(schemas[0].relations.iter().all(|relation| !relation.name.starts_with("_cf_")));
        let columns = session.columns(users.clone()).await.unwrap();
        let key = vec!["id".to_string()];
        let page = session.rows(users.clone(), columns.clone(), Vec::new(), None, key.clone(), 0, 10).await.unwrap();
        assert_eq!(page.rows[0][1].as_deref(), Some("Ada"));
        assert_eq!(page.rows[0][2].as_deref(), Some("9.5"));
        assert_eq!(page.rows[0][3].as_deref(), Some("\\xcafe"));
        assert_eq!(page.rows[2][1].as_deref(), Some("Zoë ✓"));

        let value = |text: &str| CellDraft::Value { text: text.into() };
        let changes = PendingChanges {
            inserts: vec![NewRow { values: HashMap::from([("name".into(), value("new"))]) }],
            updates: vec![RowUpdate { key: vec!["2".into()], values: HashMap::from([("name".into(), value("Grace H."))]) }],
            deletes: vec![vec!["3".into()]],
        };
        session.commit(changes, users.clone(), columns.clone(), key.clone()).await.unwrap();
        assert_eq!(session.row_count(users.clone(), columns.clone(), Vec::new()).await.unwrap(), 3);
        // A row someone else deleted: the guard fails the batch, nothing is saved.
        let stale = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["1".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["99".into()], values: HashMap::from([("name".into(), value("gone"))]) },
            ],
            ..Default::default()
        };
        let error = session.commit(stale, users.clone(), columns.clone(), key.clone()).await.unwrap_err();
        assert!(error.message().contains("someone else"), "{error}");
        let ada = session.run_query(format!("SELECT name FROM {} WHERE id = 1", users.name), 1).await.unwrap();
        assert_eq!(ada.result_sets[0].rows[0][0].as_deref(), Some("Ada"));

        // A type change rebuilds the table, foreign keys deferred, rows kept.
        let structure = session.structure(users.clone()).await.unwrap();
        let score = structure.columns.iter().find(|column| column.name == "score").unwrap().clone();
        let change = StructureChange::EditColumn {
            original: score.clone(),
            column: ColumnSpec { name: "score".into(), column_type: "NUMERIC".into(), nullable: true, default_value: Some("0".into()), comment: None },
        };
        session.apply_structure_change(users.clone(), change).await.unwrap();
        let after = session.columns(users.clone()).await.unwrap();
        assert_eq!(after.iter().find(|column| column.name == "score").unwrap().column_type, "NUMERIC");
        assert_eq!(session.row_count(users.clone(), after, Vec::new()).await.unwrap(), 3);

        // BEGIN and COMMIT are refused with a reason; a batch is the transaction.
        assert!(session.run_query("BEGIN; SELECT 1; COMMIT".into(), 1).await.unwrap_err().message().contains("one transaction"));
    })
    .await;
}

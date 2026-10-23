//! Firestore tests run against the Firestore emulator:
//! `TABLEPORT_TEST_FIRESTORE=127.0.0.1:8085 cargo test`. Without it they
//! pass without running. Each test uses a project of its own, which the
//! emulator keeps apart, and empties it afterwards.

use std::collections::HashMap;
use std::sync::Arc;

use tableport_core::{ConnectionSettings, DatabaseSession, Engine, Relation, RelationKind, TlsMode};

fn settings(project: &str) -> Option<ConnectionSettings> {
    let emulator = std::env::var("TABLEPORT_TEST_FIRESTORE").ok()?;
    Some(ConnectionSettings {
        host: emulator,
        port: 0,
        database: String::new(),
        user: project.into(),
        password: None,
        tls: TlsMode::Disable,
        ssh: None,
        read_only: false,
        engine: Engine::Firestore,
    })
}

async fn in_project<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<DatabaseSession>, Relation) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let project = format!("demo-{name}-{}", std::process::id());
    let Some(settings) = settings(&project) else { return };
    let emulator = settings.host.clone();
    let session = DatabaseSession::connect(settings).await.expect("connect");
    session
        .run_query(
            "db.collection('users').doc('ada').set({ name: 'Ada', age: 36, score: 9.5, joined: ISODate('2024-01-02T03:04:05Z'), tags: ['math', 'early'], vip: true, address: { city: 'London' } });\n\
             db.collection('users').doc('grace').set({ name: 'Grace', age: 45, score: 7, vip: false });\n\
             db.collection('users').doc('alan').set({ name: 'alan', age: 41 });\n\
             db.collection('users').doc('zoe').set({ name: 'Zoë ✓', age: 'unknown' });\n\
             db.collection('users').doc('ada').collection('orders').doc('o1').set({ total: 129.9 });\n\
             db.collection('teams').doc('core').set({ name: 'Core' })"
                .into(),
            10,
        )
        .await
        .expect("seed");
    let users = Relation { schema: "(default)".into(), name: "users".into(), kind: RelationKind::Table };
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(body(session, users))).await;
    let url = format!("http://{emulator}/emulator/v1/projects/{project}/databases/(default)/documents");
    tokio::process::Command::new("curl").args(["-s", "-g", "-X", "DELETE", &url]).output().await.ok();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn browses_collections_fields_and_pages() {
    use tableport_core::{FilterOperator, FilterTarget, RowFilter, RowSort};
    in_project("browse", |session, users| async move {
        assert_eq!(session.server_version(), "Firestore emulator");
        let schemas = session.schemas().await.unwrap();
        let names: Vec<_> = schemas[0].relations.iter().map(|relation| relation.name.as_str()).collect();
        assert_eq!(names, ["teams", "users"]);
        let columns = session.columns(users.clone()).await.unwrap();
        let summary: Vec<_> = columns.iter().map(|column| (column.name.as_str(), column.column_type.as_str())).collect();
        assert_eq!(
            summary,
            [
                ("__name__", "id"),
                ("address", "map"),
                ("age", "integer (mixed)"),
                ("joined", "timestamp"),
                ("name", "string"),
                ("score", "double (mixed)"),
                ("tags", "array"),
                ("vip", "boolean")
            ]
        );
        assert_eq!(session.primary_key(users.clone()).await.unwrap(), ["__name__"]);
        let key = vec!["__name__".to_string()];
        let page = session.rows(users.clone(), columns.clone(), Vec::new(), None, key.clone(), 0, 2).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0][0].as_deref(), Some("ada"));
        assert_eq!(page.rows[0][1].as_deref(), Some("{\"city\":\"London\"}"));
        assert_eq!(page.rows[0][3].as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(page.rows[0][6].as_deref(), Some("[\"math\",\"early\"]"));
        let next = session.rows(users.clone(), columns.clone(), Vec::new(), None, key.clone(), 2, 2).await.unwrap();
        assert_eq!(next.rows[0][0].as_deref(), Some("grace"));

        let filter = |target, operator, value: &str| RowFilter { target, operator, value: value.into() };
        let column = |name: &str| FilterTarget::Column { name: name.into() };
        let count = |filters: Vec<RowFilter>| {
            let session = session.clone();
            let relation = users.clone();
            let columns = columns.clone();
            async move { session.row_count(relation, columns, filters).await.unwrap() }
        };
        assert_eq!(count(vec![]).await, 4);
        assert_eq!(count(vec![filter(column("age"), FilterOperator::GreaterThan, "40")]).await, 2, "typed: 45 and 41");
        assert_eq!(count(vec![filter(column("name"), FilterOperator::Contains, "A")]).await, 3, "here, no case");
        assert_eq!(count(vec![filter(column("name"), FilterOperator::StartsWith, "Gr")]).await, 1);
        assert_eq!(count(vec![filter(column("score"), FilterOperator::NotEquals, "7")]).await, 3, "a missing field differs from 7");
        assert_eq!(count(vec![filter(column("vip"), FilterOperator::Equals, "true")]).await, 1);
        assert_eq!(count(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "london")]).await, 1);
        let filtered = session.rows(users.clone(), columns.clone(), vec![filter(column("name"), FilterOperator::Contains, "a")], None, key.clone(), 1, 10).await.unwrap();
        assert_eq!(filtered.rows.len(), 2);
        let sorted = session.rows(users.clone(), columns.clone(), Vec::new(), Some(RowSort { column: "age".into(), ascending: false }), key, 0, 1).await.unwrap();
        assert_eq!(sorted.rows[0][0].as_deref(), Some("zoe"), "strings sort after numbers");
        assert_eq!(session.estimated_row_count(users).await.unwrap(), Some(4));
    })
    .await;
}

#[tokio::test]
async fn commits_with_preconditions() {
    use tableport_core::{changes_sql, CellDraft, NewRow, PendingChanges, RowUpdate};
    in_project("commit", |session, users| async move {
        let columns = session.columns(users.clone()).await.unwrap();
        let key = vec!["__name__".to_string()];
        let value = |text: &str| CellDraft::Value { text: text.into() };
        let changes = PendingChanges {
            updates: vec![RowUpdate { key: vec!["grace".into()], values: HashMap::from([("age".into(), value("46")), ("score".into(), CellDraft::Null)]) }],
            deletes: vec![vec!["alan".into()]],
            inserts: vec![NewRow { values: HashMap::from([("__name__".into(), value("linus")), ("name".into(), value("Linus")), ("tags".into(), value("['kernel']"))]) }],
        };
        let review = changes_sql(&changes, &users, &columns, &key, Engine::Firestore).unwrap();
        assert_eq!(review[0], "db.collection(\"users\").doc(\"grace\").update({ \"age\": 46, \"score\": FieldValue.delete() })");
        session.commit(changes, users.clone(), columns.clone(), key.clone()).await.unwrap();
        let grace = session.run_query("db.collection('users').doc('grace').get()".into(), 10).await.unwrap();
        let set = &grace.result_sets[0];
        assert_eq!(set.rows[0][set.columns.iter().position(|name| name == "age").unwrap()].as_deref(), Some("46"));
        assert!(!set.columns.contains(&"score".to_string()), "NULL removed the field");
        let linus = session.run_query("db.collection('users').where('name', '==', 'Linus').get()".into(), 10).await.unwrap();
        assert_eq!(linus.result_sets[0].rows[0][0].as_deref(), Some("linus"));

        // A document someone else deleted: the commit fails whole.
        let stale = PendingChanges {
            updates: vec![
                RowUpdate { key: vec!["ada".into()], values: HashMap::from([("name".into(), value("changed"))]) },
                RowUpdate { key: vec!["alan".into()], values: HashMap::from([("name".into(), value("gone"))]) },
            ],
            ..Default::default()
        };
        assert!(session.commit(stale, users.clone(), columns.clone(), key.clone()).await.unwrap_err().message().contains("someone else"));
        let ada = session.full_rows(users.clone(), columns.clone(), key.clone(), vec![vec!["ada".into()]]).await.unwrap();
        assert_eq!(ada[0][columns.iter().position(|column| column.name == "name").unwrap()].as_deref(), Some("Ada"));
        // A typed value is checked against its field's type.
        let wrong = PendingChanges { updates: vec![RowUpdate { key: vec!["ada".into()], values: HashMap::from([("vip".into(), value("maybe"))]) }], ..Default::default() };
        assert!(session.commit(wrong, users.clone(), columns.clone(), key.clone()).await.is_err());
        // An ID in use is refused, not overwritten.
        let taken = PendingChanges { inserts: vec![NewRow { values: HashMap::from([("__name__".into(), value("ada"))]) }], ..Default::default() };
        assert!(session.commit(taken, users.clone(), columns, key).await.unwrap_err().message().contains("already exists"));
    })
    .await;
}

#[tokio::test]
async fn runs_sdk_chains_and_ai_reads() {
    in_project("query", |session, _users| async move {
        let result = session
            .run_query("db.collection('users').where('age', '>=', 40).orderBy('age', 'desc').limit(1).get(); db.collection('users').count(); db.listCollections(); db.collectionGroup('orders').get()".into(), 10)
            .await
            .unwrap();
        assert_eq!(result.statement_verbs, ["GET", "COUNT", "LISTCOLLECTIONS", "GET"]);
        assert_eq!(result.result_sets[0].rows[0][0].as_deref(), Some("grace"));
        assert_eq!(result.result_sets[1].rows[0][0].as_deref(), Some("4"));
        assert_eq!(result.result_sets[2].rows.len(), 2);
        assert_eq!(result.result_sets[3].rows[0][0].as_deref(), Some("users/ada/orders/o1"));
        let added = session.run_query("db.collection('logs').add({ at: 1 }); db.collection('logs').count()".into(), 1).await.unwrap();
        assert_eq!(added.result_sets[0].rows[0][0].as_deref(), Some("1"));
        assert!(session.run_query("db.collection('users').drop()".into(), 1).await.is_err());

        // Drop: every document, and those of subcollections.
        let users = Relation { schema: "(default)".into(), name: "users".into(), kind: RelationKind::Table };
        session.execute(tableport_core::drop_relation_sql(users, false, Engine::Firestore)).await.unwrap();
        let left = session.run_query("db.collection('users').count(); db.collection('users').doc('ada').collection('orders').count()".into(), 1).await.unwrap();
        assert_eq!((left.result_sets[0].rows[0][0].as_deref(), left.result_sets[1].rows[0][0].as_deref()), (Some("0"), Some("0")));

        // A read-only connection reads, and refuses writes.
        let read_only = DatabaseSession::connect(ConnectionSettings { read_only: true, ..session.settings() }).await.unwrap();
        assert!(read_only.run_query("db.collection('teams').get()".into(), 1).await.is_ok());
        assert!(read_only.run_query("db.collection('teams').doc('core').delete()".into(), 1).await.is_err());
    })
    .await;
}

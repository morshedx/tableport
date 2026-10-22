//! MongoDB tests need a MongoDB server without authentication. Run them as
//! `TABLEPORT_TEST_MONGO=127.0.0.1:27018 cargo test`; without it they pass
//! without running. Each test uses a database of its own and drops it.

use std::collections::HashMap;

use tableport_core::{
    CellDraft, ColumnKind, ConnectionSettings, DatabaseSession, Engine, FilterOperator, FilterTarget, NewRow, PendingChanges, Relation,
    RelationKind, RowFilter, RowSort, RowUpdate, StructureChange, TlsMode,
};

fn settings(server: &str, database: &str) -> ConnectionSettings {
    let (host, port) = server.rsplit_once(':').expect("host:port");
    ConnectionSettings {
        host: host.into(),
        port: port.parse().expect("a port"),
        database: database.into(),
        user: String::new(),
        password: None,
        tls: TlsMode::Disable,
        ssh: None,
        read_only: false,
        engine: Engine::Mongo,
    }
}

const SEED: &str = r#"[
  { "_id": 1, "name": "Ada", "age": 36, "joined": { "$date": "2024-01-02T03:04:05Z" }, "tags": ["math", "code"], "address": { "city": "London" } },
  { "_id": 2, "name": "Grace", "age": 45.5, "active": true },
  { "_id": 3, "name": "alan", "age": 41 },
  { "_id": 4, "name": "100% sure", "age": 7, "note": null }
]"#;

#[tokio::test]
async fn browses_filters_and_edits_documents() {
    let Ok(server) = std::env::var("TABLEPORT_TEST_MONGO") else { return };
    let database = format!("tableport_{}", std::process::id());
    let session = DatabaseSession::connect(settings(&server, &database)).await.expect("connect");
    assert!(session.server_version().starts_with("MongoDB "), "{}", session.server_version());
    let people = Relation { schema: database.clone(), name: "people".into(), kind: RelationKind::Table };

    // Seed with the driver itself, so every field has its real type.
    let documents: Vec<serde_json::Value> = serde_json::from_str(SEED).unwrap();
    let documents: Vec<mongodb::bson::Document> = documents
        .into_iter()
        .map(|document| match mongodb::bson::Bson::try_from(document).unwrap() {
            mongodb::bson::Bson::Document(document) => document,
            _ => unreachable!("an object"),
        })
        .collect();
    client(&server).await.database(&database).collection::<mongodb::bson::Document>("people").insert_many(documents).await.unwrap();

    let result = async {
        // Databases as schemas, collections as tables.
        let schemas = session.schemas().await.unwrap();
        let ours = schemas.iter().find(|schema| schema.name == database).expect("listed");
        assert_eq!(ours.relations.iter().map(|relation| relation.name.as_str()).collect::<Vec<_>>(), ["people"]);
        assert!(schemas.iter().all(|schema| !["admin", "config", "local"].contains(&schema.name.as_str())));

        // Fields from a sample, with their types.
        let columns = session.columns(people.clone()).await.unwrap();
        let summary: Vec<_> = columns.iter().map(|column| (column.name.as_str(), column.column_type.as_str())).collect();
        assert_eq!(summary[0], ("_id", "int"));
        assert!(summary.contains(&("name", "string")));
        assert!(summary.contains(&("age", "int (mixed)")), "{summary:?}");
        assert!(summary.contains(&("tags", "array")) && summary.contains(&("address", "object")));
        let joined = columns.iter().find(|column| column.name == "joined").unwrap();
        assert_eq!((joined.column_type.as_str(), joined.kind), ("date", ColumnKind::Timestamp));
        assert!(columns.iter().find(|column| column.name == "age").unwrap().is_numeric);
        assert_eq!(session.primary_key(people.clone()).await.unwrap(), ["_id"]);

        // A page, as text: dates in ISO, nested values as JSON.
        let key = vec!["_id".to_string()];
        let page = session.rows(people.clone(), columns.clone(), Vec::new(), None, key.clone(), 0, 10).await.unwrap();
        let cell = |row: usize, name: &str| page.rows[row][columns.iter().position(|column| column.name == name).unwrap()].clone();
        assert_eq!(cell(0, "name").as_deref(), Some("Ada"));
        assert_eq!(cell(0, "joined").as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(cell(0, "tags").as_deref(), Some(r#"["math","code"]"#));
        assert_eq!(cell(0, "address").as_deref(), Some(r#"{"city":"London"}"#));
        assert_eq!(cell(1, "age").as_deref(), Some("45.5"));
        assert_eq!(cell(1, "address"), None, "a missing field shows as NULL");

        // Filters.
        let count = |filters: Vec<RowFilter>| {
            let session = session.clone();
            let people = people.clone();
            let columns = columns.clone();
            async move { session.row_count(people, columns, filters).await.unwrap() }
        };
        let on = |name: &str, operator, value: &str| RowFilter { target: FilterTarget::Column { name: name.into() }, operator, value: value.into() };
        assert_eq!(count(vec![]).await, 4);
        assert_eq!(count(vec![on("name", FilterOperator::Contains, "A")]).await, 3, "Ada, Grace, alan: no case");
        assert_eq!(count(vec![on("name", FilterOperator::Contains, "%")]).await, 1, "% is literal");
        assert_eq!(count(vec![on("age", FilterOperator::GreaterThan, "40")]).await, 2, "numbers compare as numbers");
        assert_eq!(count(vec![on("age", FilterOperator::Equals, "36")]).await, 1);
        assert_eq!(count(vec![on("note", FilterOperator::IsNull, "")]).await, 4, "null and missing");
        assert_eq!(count(vec![on("active", FilterOperator::IsNotNull, "")]).await, 1);
        assert_eq!(count(vec![on("name", FilterOperator::Like, "a%")]).await, 1, "LIKE keeps case");
        assert_eq!(count(vec![on("_id", FilterOperator::In, "1, 3")]).await, 2);
        let raw = RowFilter { target: FilterTarget::RawSql, operator: FilterOperator::Equals, value: "{ age: { $lt: 40 }, name: /^a/i }".into() };
        assert_eq!(count(vec![raw]).await, 1, "only Ada is under 40 with a name starting with a");
        let any = RowFilter { target: FilterTarget::AnyColumn, operator: FilterOperator::Contains, value: "SURE".into() };
        assert_eq!(count(vec![any]).await, 1);
        let sorted = session
            .rows(people.clone(), columns.clone(), Vec::new(), Some(RowSort { column: "age".into(), ascending: false }), key.clone(), 0, 1)
            .await
            .unwrap();
        assert_eq!(sorted.rows[0][columns.iter().position(|column| column.name == "name").unwrap()].as_deref(), Some("Grace"));

        // Edits keep each field's type; NULL removes a field.
        let value = |text: &str| CellDraft::Value { text: text.into() };
        let changes = PendingChanges {
            updates: vec![RowUpdate {
                key: vec!["1".into()],
                values: HashMap::from([("age".into(), value("37")), ("tags".into(), value("['math']")), ("address".into(), CellDraft::Null)]),
            }],
            deletes: vec![vec!["4".into()]],
            inserts: vec![NewRow { values: HashMap::from([("name".into(), value("Linus")), ("age".into(), value("55"))]) }],
        };
        session.commit(changes, people.clone(), columns.clone(), key.clone()).await.unwrap();
        let page = session.rows(people.clone(), columns.clone(), Vec::new(), None, key.clone(), 0, 10).await.unwrap();
        assert_eq!(page.rows.len(), 4);
        assert_eq!(page.rows[0][columns.iter().position(|column| column.name == "age").unwrap()].as_deref(), Some("37"));
        assert_eq!(page.rows[0][columns.iter().position(|column| column.name == "address").unwrap()], None);
        assert_eq!(count(vec![on("age", FilterOperator::Equals, "37")]).await, 1, "37 was stored as a number");

        // A stale document stops the commit.
        let stale = PendingChanges { updates: vec![RowUpdate { key: vec!["99".into()], values: HashMap::from([("name".into(), value("x"))]) }], ..Default::default() };
        assert!(session.commit(stale, people.clone(), columns.clone(), key.clone()).await.unwrap_err().message().contains("someone else"));
        let wrong = PendingChanges { updates: vec![RowUpdate { key: vec!["1".into()], values: HashMap::from([("age".into(), value("old"))]) }], ..Default::default() };
        assert!(session.commit(wrong, people.clone(), columns.clone(), key.clone()).await.unwrap_err().message().contains("whole number"));

        // Full values, and indexes.
        let full = session.full_rows(people.clone(), columns.clone(), key.clone(), vec![vec!["2".into()]]).await.unwrap();
        assert_eq!(full[0][columns.iter().position(|column| column.name == "name").unwrap()].as_deref(), Some("Grace"));
        session.apply_structure_change(people.clone(), StructureChange::AddIndex { name: "by_name".into(), columns: vec!["name".into()], unique: true }).await.unwrap();
        let structure = session.structure(people.clone()).await.unwrap();
        assert!(structure.indexes.iter().any(|index| index.name == "by_name" && index.is_unique), "{:?}", structure.indexes);
        assert!(structure.indexes.iter().any(|index| index.is_primary));
        session.apply_structure_change(people.clone(), StructureChange::DropIndex { name: "by_name".into(), cascade: false }).await.unwrap();
        assert_eq!(session.structure(people.clone()).await.unwrap().indexes.len(), 1);
    };
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    drop_database(&server, &database).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn client(server: &str) -> mongodb::Client {
    mongodb::Client::with_uri_str(format!("mongodb://{server}/?directConnection=true")).await.unwrap()
}

async fn drop_database(server: &str, database: &str) {
    client(server).await.database(database).drop().await.unwrap();
}

/// One allowed MongoDB connection, for the AI access attack test.
struct Host {
    settings: ConnectionSettings,
}

impl tableport_core::McpHost for Host {
    fn connections(&self) -> Vec<tableport_core::McpConnection> {
        vec![tableport_core::McpConnection {
            id: "mongo".into(),
            name: "Mongo".into(),
            database: self.settings.database.clone(),
            all_databases: true,
            tag: None,
            engine: Engine::Mongo,
        }]
    }

    fn settings(&self, _connection_id: String) -> Option<ConnectionSettings> {
        Some(self.settings.clone())
    }

    fn record(&self, _entry: tableport_core::McpLogEntry) {}
}

#[tokio::test]
async fn runs_shell_commands_exports_and_only_reads_for_ai() {
    let Ok(server) = std::env::var("TABLEPORT_TEST_MONGO") else { return };
    let database = format!("tableport_shell_{}", std::process::id());
    let session = DatabaseSession::connect(settings(&server, &database)).await.expect("connect");
    let result = async {
        // Writes in the editor.
        let run = |text: &str| {
            let session = session.clone();
            let text = text.to_string();
            async move { session.run_query(text, 100).await }
        };
        let inserted = run("db.orders.insertMany([{ _id: 1, who: 'ada', total: 30, items: ['pen'] }, { _id: 2, who: 'grace', total: 70 }, { _id: 3, who: 'ada', total: 5.5 }])").await.unwrap();
        assert_eq!(inserted.statement_counts, [3]);
        let updated = run("db.orders.updateOne({ _id: 3 }, { $set: { total: 6 } }); db.orders.deleteOne({ _id: 99 })").await.unwrap();
        assert_eq!(updated.statement_counts, [1, 0]);
        assert_eq!(updated.statement_verbs, ["updateOne", "deleteOne"]);

        // Reads: find with modifiers, aggregate, count, distinct, show.
        let found = run("db.orders.find({ total: { $gt: 5 } }, { who: 1, total: 1 }).sort({ total: -1 }).limit(2)").await.unwrap();
        let set = &found.result_sets[0];
        assert_eq!(set.columns, ["_id", "who", "total"]);
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.rows[0][2].as_deref(), Some("70"));
        assert_eq!(set.numeric_columns, [true, false, true]);
        let paged = session.run_query("db.orders.find()".into(), 2).await.unwrap();
        assert_eq!((paged.result_sets[0].rows.len(), paged.result_sets[0].total_rows), (2, 3), "the footer counts every match");
        let grouped = run("db.orders.aggregate([{ $group: { _id: '$who', sum: { $sum: '$total' } } }, { $sort: { _id: 1 } }])").await.unwrap();
        assert_eq!(grouped.result_sets[0].rows, [[Some("ada".to_string()), Some("36".to_string())], [Some("grace".to_string()), Some("70".to_string())]]);
        let count = run("db.getCollection('orders').countDocuments({ who: 'ada' })").await.unwrap();
        assert_eq!(count.result_sets[0].rows[0][0].as_deref(), Some("2"));
        let distinct = run("db['orders'].distinct('who')").await.unwrap();
        assert_eq!(distinct.result_sets[0].rows.len(), 2);
        let collections = run("show collections").await.unwrap();
        assert_eq!(collections.result_sets[0].rows, [[Some("orders".to_string())]]);
        assert!(run("db.orders.explode()").await.unwrap_err().message().contains("explode"));
        assert!(run("SELECT * FROM orders").await.is_err());

        // Export: whole documents in relaxed extended JSON, and CSV.
        let orders = Relation { schema: database.clone(), name: "orders".into(), kind: RelationKind::Table };
        let columns = session.columns(orders.clone()).await.unwrap();
        let path = std::env::temp_dir().join(format!("tableport-mongo-{}.json", std::process::id()));
        let written = session
            .export_rows(orders.clone(), columns.clone(), Vec::new(), None, vec!["_id".into()], tableport_core::ExportScope::All,
                tableport_core::ExportFormat::Json, path.to_string_lossy().into())
            .await
            .unwrap();
        assert_eq!(written, 3);
        let exported: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(exported[0], serde_json::json!({ "_id": 1, "who": "ada", "total": 30, "items": ["pen"] }));
        std::fs::remove_file(&path).ok();

        // A read-only connection runs reads and refuses writes.
        let read_only = DatabaseSession::connect(ConnectionSettings { read_only: true, ..settings(&server, &database) }).await.unwrap();
        assert!(read_only.run_query("db.orders.find()".into(), 10).await.is_ok());
        assert!(read_only.run_query("db.orders.deleteMany({})".into(), 10).await.is_err());

        // AI access: reads work; writes, $out, $merge and commands don't.
        let socket = std::env::temp_dir().join(format!("tableport-mcp-mongo-{}.sock", std::process::id()));
        let mcp = tableport_core::start_mcp_server(socket.to_string_lossy().into(), "test".into(), std::sync::Arc::new(Host { settings: settings(&server, &database) })).unwrap();
        let (reader, mut writer) = tokio::net::UnixStream::connect(&socket).await.unwrap().into_split();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(reader).lines();
        macro_rules! call {
            ($tool:expr, $arguments:expr) => {{
                let message = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": $tool, "arguments": $arguments } });
                writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
                let reply: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                (reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(), reply["result"]["isError"].as_bool().unwrap_or(true))
            }};
        }
        let (text, error) = call!("run_read_query", serde_json::json!({ "sql": "db.orders.find({ who: 'ada' })" }));
        assert!(!error && text.contains("\"count\": 2"), "{text}");
        let (text, error) = call!("describe_table", serde_json::json!({ "schema": database, "table": "orders" }));
        assert!(!error && text.contains("who"), "{text}");
        for sql in [
            "db.orders.deleteMany({})",
            "db.orders.insertOne({ x: 1 })",
            "db.orders.updateMany({}, { $set: { total: 0 } })",
            "db.orders.drop()",
            "db.runCommand({ drop: 'orders' })",
            "db.orders.aggregate([{ $match: {} }, { $out: 'copy' }])",
            "db.orders.aggregate([{ $merge: { into: 'orders' } }])",
            "db.orders.find(); db.orders.deleteMany({})",
            "db.dropDatabase()",
        ] {
            let (text, error) = call!("run_read_query", serde_json::json!({ "sql": sql }));
            assert!(error, "not refused: {sql} → {text}");
        }
        mcp.stop();
        let after = run("db.orders.find().sort({ _id: 1 })").await.unwrap();
        assert_eq!(after.result_sets[0].rows.len(), 3);
        let names = run("show collections").await.unwrap();
        assert_eq!(names.result_sets[0].rows.len(), 1, "no copy was written");
    };
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    drop_database(&server, &database).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

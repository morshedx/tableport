mod common;

use tableport_core::{
    rows_to_csv, rows_to_insert_sql, rows_to_json, ExportFormat, ExportScope, FilterOperator, FilterTarget, Relation,
    RelationKind, RowFilter,
};
use common::{enabled, with_schema};

fn cells(values: &[Option<&str>]) -> Vec<Option<String>> {
    values.iter().map(|value| value.map(str::to_string)).collect()
}

#[test]
fn csv_quotes_only_what_needs_it() {
    let csv = rows_to_csv(
        vec!["id".into(), "note".into()],
        vec![cells(&[Some("1"), Some("plain")]), cells(&[Some("2"), Some("a, \"b\"\nc")]), cells(&[Some("3"), None])],
        true,
    );
    assert_eq!(csv, "id,note\n1,plain\n2,\"a, \"\"b\"\"\nc\"\n3,\n");
}

#[test]
fn insert_sql_quotes_values_and_names() {
    let relation = Relation { schema: "public".into(), name: "Odd \"T\"".into(), kind: RelationKind::Table };
    let sql = rows_to_insert_sql(relation, vec!["id".into(), "name".into()], vec![cells(&[Some("1"), Some("O'Brien")]), cells(&[Some("2"), None])]).unwrap();
    assert_eq!(
        sql,
        "INSERT INTO \"public\".\"Odd \"\"T\"\"\" (\"id\", \"name\") VALUES ('1', 'O''Brien');\n\
         INSERT INTO \"public\".\"Odd \"\"T\"\"\" (\"id\", \"name\") VALUES ('2', NULL);\n"
    );
}

#[test]
fn json_escapes_strings() {
    let json = rows_to_json(vec!["a".into(), "b".into()], vec![cells(&[Some("x\"y\\z\n"), None])]);
    assert_eq!(json, "[\n  {\"a\": \"x\\\"y\\\\z\\n\", \"b\": null}\n]\n");
    assert_eq!(rows_to_json(vec!["a".into()], vec![]), "[]\n");
}

const SETUP: &[&str] = &[
    "CREATE TABLE $S.t (id int PRIMARY KEY, name text, amount numeric, ok boolean, data jsonb)",
    r#"INSERT INTO $S.t VALUES
         (1, 'alice', 1.50, true, '{"k": 1}'),
         (2, 'bob, "the builder"', NULL, false, NULL),
         (3, repeat('x', 5000), 3, NULL, '[1,2]')"#,
];

fn relation(schema: &str) -> Relation {
    Relation { schema: schema.into(), name: "t".into(), kind: RelationKind::Table }
}

fn temp_path(name: &str) -> String {
    std::env::temp_dir().join(format!("tableport-export-{}-{name}", std::process::id())).to_string_lossy().into_owned()
}

#[tokio::test]
async fn exports_csv_and_json_files() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let key = vec!["id".to_string()];

        let csv_path = temp_path("all.csv");
        let count = session
            .export_rows(relation(&schema), columns.clone(), vec![], None, key.clone(), ExportScope::All, ExportFormat::Csv, csv_path.clone())
            .await
            .unwrap();
        assert_eq!(count, 3);
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        assert!(csv.starts_with("id,name,amount,ok,data\n1,alice,1.50,t,\"{\"\"k\"\": 1}\"\n2,\"bob, \"\"the builder\"\"\",,f,\n"), "{}", &csv[..200.min(csv.len())]);
        assert!(csv.contains(&"x".repeat(5000)), "long values are not cut");

        let filter = RowFilter { target: FilterTarget::Column { name: "id".into() }, operator: FilterOperator::GreaterThan, value: "1".into() };
        let json_path = temp_path("filtered.json");
        let count = session
            .export_rows(relation(&schema), columns.clone(), vec![filter], None, key.clone(), ExportScope::All, ExportFormat::Json, json_path.clone())
            .await
            .unwrap();
        assert_eq!(count, 2);
        let json = std::fs::read_to_string(&json_path).unwrap();
        assert!(json.starts_with("[\n  {\"id\":2,\"name\":\"bob, \\\"the builder\\\"\",\"amount\":null,\"ok\":false,\"data\":null},"), "{}", &json[..200.min(json.len())]);
        assert!(json.contains("\"data\":[1, 2]"), "jsonb stays JSON");

        let page_path = temp_path("page.csv");
        let count = session
            .export_rows(relation(&schema), columns.clone(), vec![], None, key.clone(), ExportScope::Page { offset: 1, limit: 1 }, ExportFormat::Csv, page_path.clone())
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(std::fs::read_to_string(&page_path).unwrap().contains("2,\"bob"));

        let keys_path = temp_path("keys.json");
        let count = session
            .export_rows(relation(&schema), columns, vec![], None, key, ExportScope::Keys { keys: vec![vec!["3".into()], vec!["1".into()]] }, ExportFormat::Json, keys_path.clone())
            .await
            .unwrap();
        assert_eq!(count, 2);
        let sql_path = temp_path("all.sql");
        let count = session
            .export_rows(relation(&schema), session.columns(relation(&schema)).await.unwrap(), vec![], None, vec!["id".into()], ExportScope::All, ExportFormat::Sql, sql_path.clone())
            .await
            .unwrap();
        assert_eq!(count, 3);
        let sql = std::fs::read_to_string(&sql_path).unwrap();
        let first = sql.lines().next().unwrap();
        assert_eq!(first, format!("INSERT INTO \"{schema}\".\"t\" (\"id\", \"name\", \"amount\", \"ok\", \"data\") VALUES ('1', 'alice', '1.50', 'true', '{{\"k\": 1}}');"));
        assert!(sql.contains("'bob, \"the builder\"', NULL, 'false', NULL);"), "NULLs stay NULL");
        // The statements load back into a copy of the table.
        session.execute(format!("CREATE TABLE {schema}.t2 (LIKE {schema}.t)")).await.unwrap();
        session.execute(sql.replace(&format!("\"{schema}\".\"t\""), &format!("\"{schema}\".\"t2\""))).await.unwrap();
        let copied = session.row_count(Relation { schema: schema.clone(), name: "t2".into(), kind: RelationKind::Table }, vec![], vec![]).await.unwrap();
        assert_eq!(copied, 3);

        for path in [csv_path, json_path, page_path, keys_path, sql_path] {
            std::fs::remove_file(path).ok();
        }
    })
    .await;
}

#[tokio::test]
async fn full_rows_are_not_cut_and_keep_key_order() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let rows = session
            .full_rows(relation(&schema), columns, vec!["id".into()], vec![vec!["3".into()], vec!["9".into()], vec!["1".into()]])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "the missing key 9 is left out");
        assert_eq!(rows[0][1].as_deref().map(str::len), Some(5000));
        assert_eq!(rows[1][1].as_deref(), Some("alice"));
    })
    .await;
}

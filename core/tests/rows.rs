mod common;

use tableport_core::{FilterOperator, FilterTarget, Relation, RelationKind, RowFilter, RowSort, GRID_CELL_LIMIT};
use common::{enabled, with_schema};

const SETUP: &[&str] = &[
    r#"CREATE TABLE $S."Odd ""Name""" (id int PRIMARY KEY, name text, note text, data jsonb)"#,
    r#"INSERT INTO $S."Odd ""Name""" VALUES
         (1, 'alice', NULL, '{"a": 1}'),
         (2, 'bob', '50% off', NULL),
         (3, 'O''Brien', 'under_score', NULL),
         (4, 'dave', repeat('x', 5000), NULL),
         (10, 'erin', '', NULL)"#,
];

fn relation(schema: &str) -> Relation {
    Relation { schema: schema.into(), name: r#"Odd "Name""#.into(), kind: RelationKind::Table }
}

fn by_id() -> Option<RowSort> {
    Some(RowSort { column: "id".into(), ascending: true })
}

fn ids(rows: &[Vec<Option<String>>]) -> Vec<String> {
    rows.iter().map(|row| row[0].clone().unwrap_or_default()).collect()
}

#[tokio::test]
async fn columns_in_table_order() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let names: Vec<_> = columns.iter().map(|c| c.name.as_str()).collect();
        let types: Vec<_> = columns.iter().map(|c| c.column_type.as_str()).collect();
        assert_eq!(names, ["id", "name", "note", "data"]);
        assert_eq!(types, ["integer", "text", "text", "jsonb"]);
    })
    .await;
}

#[tokio::test]
async fn pages_and_sorts() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let sort = Some(RowSort { column: "id".into(), ascending: false });
        let first = session.rows(relation(&schema), columns.clone(), vec![], sort.clone(), vec![], 0, 2).await.unwrap();
        let second = session.rows(relation(&schema), columns, vec![], sort, vec![], 2, 2).await.unwrap();
        assert_eq!(ids(&first.rows), ["10", "4"]);
        assert_eq!(ids(&second.rows), ["3", "2"]);
        assert_eq!(session.row_count(relation(&schema), vec![], vec![]).await.unwrap(), 5);
    })
    .await;
}

#[tokio::test]
async fn nulls_and_long_cells() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let page = session.rows(relation(&schema), columns, vec![], by_id(), vec![], 0, 10).await.unwrap();
        assert_eq!(page.rows[0][2], None);
        assert_eq!(page.rows[0][3].as_deref(), Some(r#"{"a": 1}"#));
        assert_eq!(page.rows[3][2].as_ref().map(String::len), Some(GRID_CELL_LIMIT as usize));
        assert_eq!(page.rows[4][2].as_deref(), Some(""));
    })
    .await;
}

#[tokio::test]
async fn filters() {
    if !enabled() { return; }
    use FilterOperator::*;
    let cases: &[(&str, FilterOperator, &str, &[&str])] = &[
        ("id", Equals, "3", &["3"]),
        ("id", GreaterThan, "3", &["4", "10"]),
        ("id", LessThan, "2", &["1"]),
        ("name", Equals, "O'Brien", &["3"]),
        ("note", NotEquals, "50% off", &["1", "3", "4", "10"]),
        ("note", Contains, "%", &["2"]),
        ("note", Contains, "_", &["3"]),
        ("name", Contains, "ALI", &["1"]),
        ("note", IsNull, "", &["1"]),
        ("data", IsNotNull, "", &["1"]),
        ("name", Equals, "x'; DROP TABLE t; --", &[]),
        ("id", GreaterOrEqual, "4", &["4", "10"]),
        ("id", LessOrEqual, "2", &["1", "2"]),
        ("id", In, "1, 3,10", &["1", "3", "10"]),
        ("id", NotIn, "1,2,3", &["4", "10"]),
        ("name", StartsWith, "o'b", &["3"]),
        ("name", EndsWith, "IN", &["10"]),
        ("note", NotContains, "o", &["1", "4", "10"]),
        ("name", Like, "_ob", &["2"]),
    ];
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        for (column, operator, value, expected) in cases {
            let filter = RowFilter { target: FilterTarget::Column { name: column.to_string() }, operator: *operator, value: value.to_string() };
            let page = session
                .rows(relation(&schema), columns.clone(), vec![filter.clone()], by_id(), vec![], 0, 10)
                .await
                .unwrap();
            assert_eq!(ids(&page.rows), *expected, "{filter:?}");
            let count = session.row_count(relation(&schema), columns.clone(), vec![filter.clone()]).await.unwrap();
            assert_eq!(count, expected.len() as u64, "{filter:?}");
        }
    })
    .await;
}

#[tokio::test]
async fn bad_filter_value_shows_server_message() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let filter = RowFilter { target: FilterTarget::Column { name: "id".into() }, operator: FilterOperator::Equals, value: "abc".into() };
        let error = session.rows(relation(&schema), columns, vec![filter], None, vec![], 0, 10).await.unwrap_err();
        assert!(error.message().contains("invalid input syntax for type integer"), "{error}");
    })
    .await;
}

#[tokio::test]
async fn unsorted_pages_follow_the_primary_key() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        // An update moves the row to the end of the table's storage.
        session.execute(format!(r#"UPDATE {schema}."Odd ""Name""" SET name = 'al' WHERE id = 1"#)).await.unwrap();
        let columns = session.columns(relation(&schema)).await.unwrap();
        let page = session.rows(relation(&schema), columns, vec![], None, vec!["id".into()], 0, 10).await.unwrap();
        assert_eq!(ids(&page.rows), ["1", "2", "3", "4", "10"]);
    })
    .await;
}

fn filter(target: FilterTarget, operator: FilterOperator, value: &str) -> RowFilter {
    RowFilter { target, operator, value: value.into() }
}

#[tokio::test]
async fn any_column_raw_sql_and_several_filters() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let columns = session.columns(relation(&schema)).await.unwrap();
        let page = |filters: Vec<RowFilter>| {
            let (session, columns, schema) = (session.clone(), columns.clone(), schema.clone());
            async move {
                let rows = session.rows(relation(&schema), columns.clone(), filters.clone(), by_id(), vec![], 0, 10).await.unwrap();
                let count = session.row_count(relation(&schema), columns, filters).await.unwrap();
                assert_eq!(rows.rows.len() as u64, count);
                ids(&rows.rows)
            }
        };
        // "10" is an id, "off" is in a note, '{"a": 1}' is jsonb: any column, as text.
        assert_eq!(page(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "OFF")]).await, ["2"]);
        assert_eq!(page(vec![filter(FilterTarget::AnyColumn, FilterOperator::Equals, "10")]).await, ["10"]);
        assert_eq!(page(vec![filter(FilterTarget::AnyColumn, FilterOperator::Contains, "\"a\"")]).await, ["1"]);
        assert_eq!(page(vec![filter(FilterTarget::RawSql, FilterOperator::Equals, "id % 2 = 0;")]).await, ["2", "4", "10"]);
        assert_eq!(
            page(vec![
                filter(FilterTarget::RawSql, FilterOperator::Equals, "id > 1 OR id = 1"),
                filter(FilterTarget::Column { name: "note".into() }, FilterOperator::IsNotNull, ""),
            ])
            .await,
            ["2", "3", "4", "10"],
            "the OR stays inside its parentheses"
        );
        let error = session
            .rows(relation(&schema), columns.clone(), vec![filter(FilterTarget::RawSql, FilterOperator::Equals, "  ")], None, vec![], 0, 10)
            .await
            .unwrap_err();
        assert!(error.message().contains("Raw SQL filter is empty"), "{error}");
    })
    .await;
}

#[tokio::test]
async fn estimates_rows_from_statistics() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        // Never analyzed: no estimate.
        assert_eq!(session.estimated_row_count(relation(&schema)).await.unwrap(), None);
        session.execute(format!(r#"ANALYZE {schema}."Odd ""Name""""#)).await.unwrap();
        assert_eq!(session.estimated_row_count(relation(&schema)).await.unwrap(), Some(5));
    })
    .await;
}

mod common;

use std::collections::HashMap;

use tableport_core::{changes_sql, insert_sql, CellDraft, Column, NewRow, PendingChanges, Relation, RelationKind, RowUpdate};
use common::{enabled, with_schema};

fn value(text: &str) -> CellDraft {
    CellDraft::Value { text: text.into() }
}

fn row(values: &[(&str, CellDraft)]) -> NewRow {
    NewRow { values: values.iter().cloned().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>() }
}

#[test]
fn insert_sql_leaves_unset_columns_to_default() {
    let relation = Relation { schema: "public".into(), name: "users".into(), kind: RelationKind::Table };
    let columns: Vec<Column> = ["id", "name", "note"]
        .iter()
        .map(|name| Column::plain(*name, "text"))
        .collect();
    let sql = insert_sql(&row(&[("name", value("O'Brien")), ("note", CellDraft::Null)]), &relation, &columns).unwrap();
    assert_eq!(sql, r#"INSERT INTO "public"."users" ("name", "note") VALUES ('O''Brien', NULL)"#);
    let empty = insert_sql(&NewRow::default(), &relation, &columns).unwrap();
    assert_eq!(empty, r#"INSERT INTO "public"."users" DEFAULT VALUES"#);
}

const SETUP: &[&str] = &[
    "CREATE TABLE $S.teams (id int PRIMARY KEY, name text NOT NULL)",
    "CREATE TABLE $S.users (id serial PRIMARY KEY, name text NOT NULL DEFAULT 'anon',
       team_id int REFERENCES $S.teams (id), created date DEFAULT '2026-01-01')",
    "INSERT INTO $S.teams VALUES (7, 'core')",
];

#[tokio::test]
async fn inserts_with_defaults_and_nulls() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let users = Relation { schema: schema.clone(), name: "users".into(), kind: RelationKind::Table };
        let columns = session.columns(users.clone()).await.unwrap();
        session
            .insert(
                vec![row(&[("name", value("O'Brien")), ("team_id", value("7"))]), row(&[("team_id", CellDraft::Null)])],
                users.clone(),
                columns.clone(),
            )
            .await
            .unwrap();
        let page = session
            .rows(users, columns, vec![], Some(tableport_core::RowSort { column: "id".into(), ascending: true }), vec![], 0, 10)
            .await
            .unwrap();
        let some = |text: &str| Some(text.to_string());
        assert_eq!(
            page.rows,
            vec![
                vec![some("1"), some("O'Brien"), some("7"), some("2026-01-01")],
                vec![some("2"), some("anon"), None, some("2026-01-01")],
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn failed_insert_rolls_back_all_and_session_stays_usable() {
    if !enabled() { return; }
    with_schema(SETUP, |session, schema| async move {
        let teams = Relation { schema, name: "teams".into(), kind: RelationKind::Table };
        let columns = session.columns(teams.clone()).await.unwrap();
        let error = session
            .insert(
                vec![row(&[("id", value("1")), ("name", value("a"))]), row(&[("id", value("1")), ("name", value("b"))])],
                teams.clone(),
                columns.clone(),
            )
            .await
            .unwrap_err();
        assert!(error.message().contains("duplicate key"), "{error}");
        assert_eq!(session.row_count(teams.clone(), vec![], vec![]).await.unwrap(), 1, "only the setup row");
        session.insert(vec![row(&[("id", value("2")), ("name", value("c"))])], teams.clone(), columns).await.unwrap();
        assert_eq!(session.row_count(teams, vec![], vec![]).await.unwrap(), 2);
    })
    .await;
}

fn key(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

const PEOPLE: &[&str] = &[
    "CREATE TABLE $S.people (org int, id int, name text NOT NULL, note text, PRIMARY KEY (org, id))",
    "INSERT INTO $S.people VALUES (1, 1, 'ann', 'a'), (1, 2, 'bo', NULL), (2, 1, 'cy', 'c')",
];

async fn names(session: &tableport_core::DatabaseSession, people: &Relation) -> Vec<Vec<Option<String>>> {
    let columns = session.columns(people.clone()).await.unwrap();
    let sort = Some(tableport_core::RowSort { column: "name".into(), ascending: true });
    session.rows(people.clone(), columns, vec![], sort, vec![], 0, 10).await.unwrap().rows
}

#[tokio::test]
async fn reads_composite_primary_key_in_order() {
    if !enabled() { return; }
    with_schema(PEOPLE, |session, schema| async move {
        let people = Relation { schema, name: "people".into(), kind: RelationKind::Table };
        assert_eq!(session.primary_key(people).await.unwrap(), ["org", "id"]);
    })
    .await;
}

#[tokio::test]
async fn commits_updates_deletes_and_inserts_together() {
    if !enabled() { return; }
    with_schema(PEOPLE, |session, schema| async move {
        let people = Relation { schema, name: "people".into(), kind: RelationKind::Table };
        let columns = session.columns(people.clone()).await.unwrap();
        let primary_key = session.primary_key(people.clone()).await.unwrap();
        let changes = PendingChanges {
            updates: vec![RowUpdate {
                key: key(&["1", "1"]),
                values: [("name".to_string(), value("an$$n 'q'")), ("note".to_string(), CellDraft::Null)].into(),
            }],
            deletes: vec![key(&["2", "1"])],
            inserts: vec![row(&[("org", value("3")), ("id", value("1")), ("name", value("dee"))])],
        };
        session.commit(changes, people.clone(), columns, primary_key).await.unwrap();
        let some = |text: &str| Some(text.to_string());
        assert_eq!(
            names(&session, &people).await,
            vec![
                vec![some("1"), some("1"), some("an$$n 'q'"), None],
                vec![some("1"), some("2"), some("bo"), None],
                vec![some("3"), some("1"), some("dee"), None],
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn missing_row_applies_nothing() {
    if !enabled() { return; }
    with_schema(PEOPLE, |session, schema| async move {
        let people = Relation { schema, name: "people".into(), kind: RelationKind::Table };
        let columns = session.columns(people.clone()).await.unwrap();
        let primary_key = session.primary_key(people.clone()).await.unwrap();
        let before = names(&session, &people).await;
        let changes = PendingChanges {
            updates: vec![RowUpdate { key: key(&["1", "1"]), values: [("name".to_string(), value("zed"))].into() }],
            // Already gone: deleted by "someone else".
            deletes: vec![key(&["9", "9"])],
            inserts: vec![],
        };
        let error = session.commit(changes, people.clone(), columns, primary_key).await.unwrap_err();
        assert!(error.message().contains("changed or deleted by someone else"), "{error}");
        assert_eq!(names(&session, &people).await, before, "the update was rolled back too");
    })
    .await;
}

#[tokio::test]
async fn changes_without_primary_key_are_refused() {
    if !enabled() { return; }
    with_schema(&["CREATE TABLE $S.logs (line text)"], |session, schema| async move {
        let logs = Relation { schema, name: "logs".into(), kind: RelationKind::Table };
        let columns = session.columns(logs.clone()).await.unwrap();
        let changes = PendingChanges { deletes: vec![key(&["x"])], ..Default::default() };
        let error = session.commit(changes, logs, columns, vec![]).await.unwrap_err();
        assert!(error.message().contains("without a primary key"), "{error}");
    })
    .await;
}

#[test]
fn changes_sql_orders_updates_deletes_inserts() {
    let relation = Relation { schema: "s".into(), name: "t".into(), kind: RelationKind::Table };
    let columns = vec![
        Column::plain("id", "integer"),
        Column::plain("name", "text"),
    ];
    let changes = PendingChanges {
        inserts: vec![row(&[("name", value("new"))])],
        updates: vec![RowUpdate { key: key(&["1"]), values: [("name".to_string(), value("x"))].into() }],
        deletes: vec![key(&["2"])],
    };
    let statements = changes_sql(&changes, &relation, &columns, &["id".to_string()]).unwrap();
    assert_eq!(
        statements,
        [
            r#"UPDATE "s"."t" SET "name" = 'x' WHERE "id" = '1'"#,
            r#"DELETE FROM "s"."t" WHERE "id" = '2'"#,
            r#"INSERT INTO "s"."t" ("name") VALUES ('new')"#,
        ]
    );
}

const EVENTS: &[&str] = &[
    "CREATE TYPE $S.mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TABLE $S.events (
       id int PRIMARY KEY,
       day date DEFAULT '2026-01-01',
       at time,
       local_at timestamp,
       happened_at timestamptz DEFAULT '2000-01-01 00:00:00+00',
       done bool,
       mood $S.mood,
       note text
     )",
    "INSERT INTO $S.events (id, day, happened_at, mood) VALUES (1, '2025-05-05', '2025-05-05 10:00:00+00', 'sad')",
];

#[tokio::test]
async fn columns_report_kind_and_enum_values() {
    if !enabled() { return; }
    with_schema(EVENTS, |session, schema| async move {
        let events = Relation { schema, name: "events".into(), kind: RelationKind::Table };
        let columns = session.columns(events).await.unwrap();
        let kinds: Vec<_> = columns.iter().map(|c| c.kind).collect();
        use tableport_core::ColumnKind::*;
        assert_eq!(kinds, [Other, Date, Time, Timestamp, TimestampTz, Boolean, Enum, Other]);
        assert_eq!(columns[6].enum_values, ["sad", "ok", "happy"]);
        assert!(columns[0].enum_values.is_empty());
    })
    .await;
}

#[tokio::test]
async fn default_and_functions_are_sql_not_text() {
    if !enabled() { return; }
    with_schema(EVENTS, |session, schema| async move {
        let events = Relation { schema: schema.clone(), name: "events".into(), kind: RelationKind::Table };
        let columns = session.columns(events.clone()).await.unwrap();
        let changes = PendingChanges {
            updates: vec![RowUpdate {
                key: vec!["1".into()],
                values: [
                    ("day".to_string(), CellDraft::Default),
                    ("happened_at".to_string(), CellDraft::Function { function: tableport_core::SqlFunction::Now }),
                    ("mood".to_string(), value("happy")),
                ]
                .into(),
            }],
            inserts: vec![row(&[
                ("id", value("2")),
                ("day", CellDraft::Function { function: tableport_core::SqlFunction::CurrentDate }),
                ("happened_at", CellDraft::Default),
            ])],
            deletes: vec![],
        };
        session.commit(changes, events, columns, vec!["id".into()]).await.unwrap();
        let check = session
            .run_query(
                format!(
                    "SELECT id, day = '2026-01-01', happened_at > now() - interval '1 minute', mood FROM {schema}.events WHERE id = 1
                     UNION ALL
                     SELECT id, day = CURRENT_DATE, happened_at = '2000-01-01 00:00:00+00', mood FROM {schema}.events WHERE id = 2"
                ),
                10,
            )
            .await
            .unwrap();
        let some = |text: &str| Some(text.to_string());
        assert_eq!(
            check.result_sets[0].rows,
            vec![vec![some("1"), some("t"), some("t"), some("happy")], vec![some("2"), some("t"), some("t"), None]]
        );
    })
    .await;
}

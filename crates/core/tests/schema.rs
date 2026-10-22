mod common;

use tableport_core::{ForeignKey, Relation, RelationKind};
use common::{enabled, with_schema};

#[tokio::test]
async fn lists_relations_by_kind_and_databases() {
    if !enabled() { return; }
    with_schema(
        &[
            "CREATE TABLE $S.users (id int PRIMARY KEY)",
            "CREATE VIEW $S.active_users AS SELECT id FROM $S.users",
            "CREATE MATERIALIZED VIEW $S.user_count AS SELECT count(*) FROM $S.users",
            "CREATE TABLE $S.events (at date) PARTITION BY RANGE (at)",
            "CREATE TABLE $S.events_2026 PARTITION OF $S.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
        ],
        |session, schema| async move {
            let schemas = session.schemas().await.unwrap();
            let contents = schemas.iter().find(|contents| contents.name == schema).unwrap();
            let names: Vec<_> = contents.relations.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["active_users", "events", "user_count", "users"]);
            let kinds: Vec<_> = contents.relations.iter().map(|r| r.kind).collect();
            assert_eq!(
                kinds,
                [RelationKind::View, RelationKind::PartitionedTable, RelationKind::MaterializedView, RelationKind::Table]
            );
            assert!(!schemas.iter().any(|s| s.name == "pg_catalog" || s.name == "information_schema"));

            let databases = session.databases().await.unwrap();
            assert!(databases.contains(&"postgres".to_string()));
            assert!(!databases.contains(&"template0".to_string()));
        },
    )
    .await;
}

#[tokio::test]
async fn reads_single_column_foreign_keys() {
    if !enabled() { return; }
    with_schema(
        &[
            "CREATE TABLE $S.teams (id int PRIMARY KEY)",
            "CREATE TABLE $S.users (id int PRIMARY KEY, team_id int REFERENCES $S.teams (id))",
        ],
        |session, schema| async move {
            let users = Relation { schema: schema.clone(), name: "users".into(), kind: RelationKind::Table };
            let keys = session.foreign_keys(users).await.unwrap();
            assert_eq!(
                keys,
                [ForeignKey {
                    column: "team_id".into(),
                    referenced_relation: Relation { schema, name: "teams".into(), kind: RelationKind::Table },
                    referenced_column: "id".into(),
                }]
            );
        },
    )
    .await;
}

#[tokio::test]
async fn lists_routines_and_reads_definition() {
    if !enabled() { return; }
    with_schema(
        &[
            "CREATE FUNCTION $S.add_one(x integer) RETURNS integer LANGUAGE sql AS 'SELECT x + 1'",
            "CREATE PROCEDURE $S.noop(note text) LANGUAGE sql AS 'SELECT 1'",
            "CREATE AGGREGATE $S.my_sum(integer) (SFUNC = int4pl, STYPE = integer)",
        ],
        |session, schema| async move {
            let routines = session.routines(schema.clone()).await.unwrap();
            let names: Vec<_> = routines.iter().map(|r| (r.name.as_str(), r.arguments.as_str(), r.is_procedure)).collect();
            assert_eq!(names, [("add_one", "x integer", false), ("noop", "IN note text", true)]);
            let definition = session.routine_definition(routines[0].oid).await.unwrap();
            assert!(definition.starts_with("CREATE OR REPLACE FUNCTION"), "{definition}");
            assert!(definition.contains("x + 1"), "{definition}");
        },
    )
    .await;
}

#[test]
fn drop_and_truncate_sql() {
    use tableport_core::{drop_relation_sql, drop_routine_sql, refresh_materialized_view_sql, truncate_sql, Engine, Routine};
    let table = Relation { schema: "public".into(), name: r#"Odd "Name""#.into(), kind: RelationKind::Table };
    assert_eq!(drop_relation_sql(table.clone(), false, Engine::Postgres), r#"DROP TABLE "public"."Odd ""Name""";"#);
    assert_eq!(drop_relation_sql(table.clone(), true, Engine::Postgres), r#"DROP TABLE "public"."Odd ""Name""" CASCADE;"#);
    assert_eq!(truncate_sql(table.clone(), true, true, Engine::Postgres), r#"TRUNCATE "public"."Odd ""Name""" RESTART IDENTITY CASCADE;"#);
    let view = Relation { schema: "s".into(), name: "v".into(), kind: RelationKind::MaterializedView };
    assert_eq!(drop_relation_sql(view.clone(), false, Engine::Postgres), r#"DROP MATERIALIZED VIEW "s"."v";"#);
    assert_eq!(refresh_materialized_view_sql(view), r#"REFRESH MATERIALIZED VIEW "s"."v";"#);
    let routine = Routine { oid: 1, schema: "public".into(), name: "score".into(), arguments: "a integer, b text".into(), is_procedure: false };
    assert_eq!(drop_routine_sql(routine, false), r#"DROP FUNCTION "public"."score"(a integer, b text);"#);
}

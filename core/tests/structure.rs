mod common;

use tableport_core::{ColumnDetail, ConstraintKind, Relation, RelationKind};
use common::{enabled, with_schema};

#[tokio::test]
async fn reads_columns_indexes_and_constraints() {
    if !enabled() { return; }
    with_schema(
        &[
            "CREATE TABLE $S.teams (id int PRIMARY KEY)",
            "CREATE TABLE $S.users (
               id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               email varchar(255) NOT NULL UNIQUE,
               team_id int REFERENCES $S.teams (id) ON DELETE CASCADE,
               age int CHECK (age >= 0),
               created date DEFAULT '2026-01-01',
               email_lower text GENERATED ALWAYS AS (lower(email)) STORED
             )",
            "COMMENT ON COLUMN $S.users.email IS 'login address'",
            "CREATE INDEX users_created_idx ON $S.users (created)",
        ],
        |session, schema| async move {
            let users = Relation { schema, name: "users".into(), kind: RelationKind::Table };
            let structure = session.structure(users).await.unwrap();

            let column = |name: &str| structure.columns.iter().find(|c| c.name == name).cloned().unwrap();
            assert_eq!(structure.columns.len(), 6);
            assert_eq!(
                column("email"),
                ColumnDetail {
                    name: "email".into(),
                    column_type: "character varying(255)".into(),
                    nullable: false,
                    default_value: None,
                    comment: Some("login address".into()),
                }
            );
            assert_eq!(column("id").default_value.as_deref(), Some("GENERATED ALWAYS AS IDENTITY"));
            assert_eq!(column("created").default_value.as_deref(), Some("'2026-01-01'::date"));
            assert_eq!(column("email_lower").default_value.as_deref(), Some("GENERATED ALWAYS AS (lower((email)::text)) STORED"));
            assert!(column("team_id").nullable);

            let index_names: Vec<_> = structure.indexes.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(index_names, ["users_pkey", "users_created_idx", "users_email_key"]);
            assert!(structure.indexes[0].is_primary && structure.indexes[0].is_unique);
            assert!(structure.indexes[2].is_unique && !structure.indexes[2].is_primary);

            let kinds: Vec<_> = structure.constraints.iter().map(|c| c.kind).collect();
            assert_eq!(
                kinds,
                [ConstraintKind::PrimaryKey, ConstraintKind::ForeignKey, ConstraintKind::Unique, ConstraintKind::Check]
            );
            let foreign = &structure.constraints[1];
            assert!(foreign.definition.contains("REFERENCES") && foreign.definition.contains("ON DELETE CASCADE"), "{foreign:?}");
            assert_eq!(structure.constraints[3].definition, "CHECK ((age >= 0))");
        },
    )
    .await;
}

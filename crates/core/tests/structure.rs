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

mod changes {
    use super::common::{enabled, with_schema};
    use tableport_core::{structure_change_sql, ColumnDetail, ColumnSpec, Engine, Relation, RelationKind, StructureChange};

    fn users(schema: &str) -> Relation {
        Relation { schema: schema.into(), name: "users".into(), kind: RelationKind::Table }
    }

    fn spec(name: &str, column_type: &str, nullable: bool, default_value: Option<&str>) -> ColumnSpec {
        ColumnSpec { name: name.into(), column_type: column_type.into(), nullable, default_value: default_value.map(Into::into), comment: None }
    }

    #[test]
    fn edit_column_changes_only_what_differs() {
        let original = ColumnDetail { name: "age".into(), column_type: "text".into(), nullable: true, default_value: None, comment: None };
        let sql = structure_change_sql(
            users("public"),
            StructureChange::EditColumn { original, column: spec("years", "integer", false, Some("0")) },
            Engine::Postgres,
        )
        .unwrap();
        assert_eq!(sql, [
            r#"ALTER TABLE "public"."users" RENAME COLUMN "age" TO "years";"#,
            r#"ALTER TABLE "public"."users" ALTER COLUMN "years" TYPE integer USING "years"::integer;"#,
            r#"ALTER TABLE "public"."users" ALTER COLUMN "years" SET NOT NULL;"#,
            r#"ALTER TABLE "public"."users" ALTER COLUMN "years" SET DEFAULT 0;"#,
        ]);
        // Nothing changed: nothing to run.
        let same = ColumnDetail { name: "id".into(), column_type: "integer".into(), nullable: false, default_value: Some("GENERATED ALWAYS AS IDENTITY".into()), comment: None };
        let unchanged = StructureChange::EditColumn { original: same, column: spec("id", "integer", false, Some("GENERATED ALWAYS AS IDENTITY")) };
        assert!(structure_change_sql(users("public"), unchanged, Engine::Postgres).unwrap().is_empty());
    }

    #[test]
    fn add_column_index_and_foreign_key_sql() {
        let mut column = spec("note", "varchar(255)", false, Some("''"));
        column.comment = Some("Shown on the invoice".into());
        assert_eq!(structure_change_sql(users("s"), StructureChange::AddColumn { column }, Engine::Postgres).unwrap(), [
            r#"ALTER TABLE "s"."users" ADD COLUMN "note" varchar(255) NOT NULL DEFAULT '';"#,
            r#"COMMENT ON COLUMN "s"."users"."note" IS 'Shown on the invoice';"#,
        ]);
        let index = StructureChange::AddIndex { name: "users_email_idx".into(), columns: vec!["email".into()], unique: true };
        assert_eq!(structure_change_sql(users("s"), index, Engine::Postgres).unwrap(), [r#"CREATE UNIQUE INDEX "users_email_idx" ON "s"."users" ("email");"#]);
        let teams = Relation { schema: "s".into(), name: "teams".into(), kind: RelationKind::Table };
        let key = StructureChange::AddForeignKey {
            name: None, column: "team_id".into(), referenced: teams, referenced_column: "id".into(), on_delete: Some("set null".into()),
        };
        assert_eq!(structure_change_sql(users("s"), key, Engine::Postgres).unwrap(), [
            r#"ALTER TABLE "s"."users" ADD FOREIGN KEY ("team_id") REFERENCES "s"."teams" ("id") ON DELETE SET NULL;"#,
        ]);
        let bad = StructureChange::AddColumn { column: spec(" ", "text", true, None) };
        assert!(structure_change_sql(users("s"), bad, Engine::Postgres).is_err());
    }

    #[tokio::test]
    async fn changes_apply_together_or_not_at_all() {
        if !enabled() { return; }
        with_schema(&["CREATE TABLE $S.users (id int PRIMARY KEY, age text)", "INSERT INTO $S.users VALUES (1, '42')"], |session, schema| async move {
            let original = session.structure(users(&schema)).await.unwrap().columns[1].clone();
            session
                .apply_structure_change(users(&schema), StructureChange::EditColumn { original: original.clone(), column: spec("years", "integer", false, Some("0")) })
                .await
                .unwrap();
            let column = session.structure(users(&schema)).await.unwrap().columns[1].clone();
            assert_eq!((column.name.as_str(), column.column_type.as_str(), column.nullable, column.default_value.as_deref()), ("years", "integer", false, Some("0")));

            // The second statement fails (no such type): the rename before it is undone.
            let renamed = session.structure(users(&schema)).await.unwrap().columns[1].clone();
            let failing = StructureChange::EditColumn { original: renamed, column: spec("renamed", "no_such_type", false, Some("0")) };
            assert!(session.apply_structure_change(users(&schema), failing).await.is_err());
            assert_eq!(session.structure(users(&schema)).await.unwrap().columns[1].name, "years");
            // And the session still works.
            session.apply_structure_change(users(&schema), StructureChange::DropColumn { name: "years".into(), cascade: false }).await.unwrap();
            assert_eq!(session.structure(users(&schema)).await.unwrap().columns.len(), 1);
        })
        .await;
    }
}

mod create {
    use super::common::{enabled, with_schema};
    use tableport_core::{create_table_sql, ColumnSpec, Engine, NewTableColumn, Relation, RelationKind};

    fn column(name: &str, column_type: &str, nullable: bool, default_value: Option<&str>, primary_key: bool) -> NewTableColumn {
        NewTableColumn {
            column: ColumnSpec { name: name.into(), column_type: column_type.into(), nullable, default_value: default_value.map(Into::into), comment: None },
            primary_key,
        }
    }

    #[test]
    fn create_table_sql_with_key_defaults_and_comments() {
        let mut email = column("email", "text", false, None, false);
        email.column.comment = Some("Login name".into());
        let sql = create_table_sql("public".into(), "Users".into(), vec![
            column("id", "bigserial", false, None, true),
            email,
            column("created_at", "timestamptz", false, Some("now()"), false),
        ], Engine::Postgres)
        .unwrap();
        assert_eq!(sql, [
            "CREATE TABLE \"public\".\"Users\" (\n  \"id\" bigserial,\n  \"email\" text NOT NULL,\n  \"created_at\" timestamptz NOT NULL DEFAULT now(),\n  PRIMARY KEY (\"id\")\n);",
            "COMMENT ON COLUMN \"public\".\"Users\".\"email\" IS 'Login name';",
        ]);
        assert!(create_table_sql("public".into(), "t".into(), vec![column("a", "int", true, None, false), column("a", "int", true, None, false)], Engine::Postgres).is_err());
        assert!(create_table_sql("public".into(), " ".into(), vec![column("a", "int", true, None, false)], Engine::Postgres).is_err());
        assert!(create_table_sql("public".into(), "t".into(), vec![], Engine::Postgres).is_err());
    }

    #[tokio::test]
    async fn creates_a_table_with_a_composite_key() {
        if !enabled() { return; }
        with_schema(&[], |session, schema| async move {
            session
                .create_table(schema.clone(), "memberships".into(), vec![
                    column("user_id", "integer", false, None, true),
                    column("team_id", "integer", false, None, true),
                    column("role", "text", false, Some("'member'"), false),
                ])
                .await
                .unwrap();
            let relation = Relation { schema: schema.clone(), name: "memberships".into(), kind: RelationKind::Table };
            assert_eq!(session.primary_key(relation.clone()).await.unwrap(), ["user_id", "team_id"]);
            let role = &session.structure(relation).await.unwrap().columns[2];
            assert_eq!((role.nullable, role.default_value.as_deref()), (false, Some("'member'::text")));
        })
        .await;
    }
}

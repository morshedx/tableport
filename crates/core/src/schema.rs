use crate::rows::{quote_identifier, quote_literal, Dialect};
use crate::{DatabaseError, DatabaseSession};

/// The kinds of relation the sidebar lists, from `pg_class.relkind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum RelationKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    fn from_relkind(code: &str) -> Option<Self> {
        match code {
            "r" => Some(Self::Table),
            "p" => Some(Self::PartitionedTable),
            "v" => Some(Self::View),
            "m" => Some(Self::MaterializedView),
            "f" => Some(Self::ForeignTable),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
}

impl Relation {
    /// `"schema"."name"`.
    pub(crate) fn quoted_name(&self) -> String {
        format!("{}.{}", quote_identifier(&self.schema), quote_identifier(&self.name))
    }

    /// The name as a regclass literal, for catalog queries.
    pub(crate) fn regclass(&self) -> Result<String, DatabaseError> {
        Ok(format!("{}::regclass", quote_literal(&self.quoted_name())?))
    }
}

/// One schema and the relations in it, sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SchemaContents {
    pub name: String,
    pub relations: Vec<Relation>,
}

/// A single-column foreign key: `column` points at `referenced_column` of
/// `referenced_relation`. Multi-column keys are left out; the filter that
/// following a key uses works on one column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ForeignKey {
    pub column: String,
    pub referenced_relation: Relation,
    pub referenced_column: String,
}

#[uniffi::export]
impl DatabaseSession {
    /// The databases the user can connect to, sorted by name.
    pub async fn databases(&self) -> Result<Vec<String>, DatabaseError> {
        // A SQLite connection is one file.
        if self.dialect() == Dialect::Sqlite {
            return Ok(vec![self.settings().database]);
        }
        let rows = self
            .text_rows(
                "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname".into(),
            )
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    /// The user's schemas with their tables, views and materialized views.
    /// System schemas are left out; schemas without relations are kept.
    pub async fn schemas(&self) -> Result<Vec<SchemaContents>, DatabaseError> {
        if self.dialect() == Dialect::Sqlite {
            return self.sqlite_schemas().await;
        }
        let schema_rows = self
            .text_rows(
                r"SELECT nspname FROM pg_namespace
                  WHERE nspname NOT IN ('pg_catalog', 'information_schema')
                    AND nspname NOT LIKE 'pg\_toast%' AND nspname NOT LIKE 'pg\_temp\_%'
                  ORDER BY nspname"
                    .into(),
            )
            .await?;
        let mut schemas: Vec<SchemaContents> = schema_rows
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .map(|name| SchemaContents { name, relations: Vec::new() })
            .collect();

        let relation_rows = self
            .text_rows(
                "SELECT n.nspname, c.relname, c.relkind
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition
                 ORDER BY n.nspname, c.relname"
                    .into(),
            )
            .await?;
        for row in relation_rows {
            let [Some(schema), Some(name), Some(kind)] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else {
                continue;
            };
            let Some(kind) = RelationKind::from_relkind(&kind) else { continue };
            if let Some(contents) = schemas.iter_mut().find(|contents| contents.name == schema) {
                contents.relations.push(Relation { schema, name, kind });
            }
        }
        Ok(schemas)
    }

    /// The primary key columns, in key order; empty without a primary key.
    pub async fn primary_key(&self, relation: Relation) -> Result<Vec<String>, DatabaseError> {
        if self.dialect() == Dialect::Sqlite {
            return self.sqlite_primary_key(&relation).await;
        }
        let rows = self
            .text_rows(format!(
                "SELECT a.attname
                 FROM pg_index i
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey::int2[])
                 WHERE i.indrelid = {} AND i.indisprimary
                 ORDER BY array_position(i.indkey::int2[], a.attnum)",
                relation.regclass()?
            ))
            .await?;
        Ok(rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    pub async fn foreign_keys(&self, relation: Relation) -> Result<Vec<ForeignKey>, DatabaseError> {
        if self.dialect() == Dialect::Sqlite {
            return self.sqlite_foreign_keys(&relation).await;
        }
        let rows = self
            .text_rows(format!(
                "SELECT a.attname, rn.nspname, rc.relname, rc.relkind, ra.attname
                 FROM pg_constraint con
                 JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[1]
                 JOIN pg_class rc ON rc.oid = con.confrelid
                 JOIN pg_namespace rn ON rn.oid = rc.relnamespace
                 JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = con.confkey[1]
                 WHERE con.conrelid = {} AND con.contype = 'f' AND cardinality(con.conkey) = 1
                 ORDER BY con.conname",
                relation.regclass()?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(column), Some(schema), Some(name), Some(kind), Some(referenced_column)] =
                    <[Option<String>; 5]>::try_from(row).ok()?
                else {
                    return None;
                };
                let kind = RelationKind::from_relkind(&kind).unwrap_or(RelationKind::Table);
                Some(ForeignKey { column, referenced_relation: Relation { schema, name, kind }, referenced_column })
            })
            .collect())
    }
}

/// A function or procedure, for the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct Routine {
    /// The pg_proc oid, to read the definition later.
    pub oid: u32,
    pub schema: String,
    pub name: String,
    /// The identity arguments, for example "integer, text".
    pub arguments: String,
    pub is_procedure: bool,
}

#[uniffi::export]
impl DatabaseSession {
    /// The schema's own functions and procedures; aggregates, window
    /// functions and those that extensions installed are left out.
    pub async fn routines(&self, schema: String) -> Result<Vec<Routine>, DatabaseError> {
        // SQLite has no stored functions.
        if self.dialect() == Dialect::Sqlite {
            return Ok(Vec::new());
        }
        let rows = self
            .text_rows(format!(
                "SELECT p.oid, p.proname, pg_get_function_identity_arguments(p.oid), p.prokind
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = {} AND p.prokind IN ('f', 'p')
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e')
                 ORDER BY p.proname, p.oid",
                quote_literal(&schema)?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(oid), Some(name), Some(arguments), Some(kind)] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                Some(Routine { oid: oid.parse().ok()?, schema: schema.clone(), name, arguments, is_procedure: kind == "p" })
            })
            .collect())
    }

    /// `CREATE OR REPLACE FUNCTION …`, as pg_get_functiondef prints it.
    pub async fn routine_definition(&self, oid: u32) -> Result<String, DatabaseError> {
        let rows = self.text_rows(format!("SELECT pg_get_functiondef({oid})")).await?;
        rows.into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten())
            .ok_or_else(|| DatabaseError::new("The function no longer exists."))
    }
}

/// `"schema"."name"`, for copying or pasting into SQL.
#[uniffi::export]
pub fn qualified_name(relation: Relation) -> String {
    relation.quoted_name()
}

/// `DROP TABLE|VIEW|MATERIALIZED VIEW|FOREIGN TABLE "schema"."name"`.
#[uniffi::export]
pub fn drop_relation_sql(relation: Relation, cascade: bool, engine: crate::Engine) -> String {
    // SQLite has no CASCADE: foreign keys decide what happens to other rows.
    let cascade = cascade && engine == crate::Engine::Postgres;
    let kind = match relation.kind {
        RelationKind::Table | RelationKind::PartitionedTable => "TABLE",
        RelationKind::View => "VIEW",
        RelationKind::MaterializedView => "MATERIALIZED VIEW",
        RelationKind::ForeignTable => "FOREIGN TABLE",
    };
    format!("DROP {kind} {}{};", relation.quoted_name(), if cascade { " CASCADE" } else { "" })
}

/// `TRUNCATE "schema"."name"`; `restart_identity` resets its sequences.
#[uniffi::export]
pub fn truncate_sql(relation: Relation, restart_identity: bool, cascade: bool, engine: crate::Engine) -> String {
    // SQLite has no TRUNCATE; DELETE without WHERE empties the table quickly.
    if engine == crate::Engine::Sqlite {
        return format!("DELETE FROM {};", relation.quoted_name());
    }
    format!(
        "TRUNCATE {}{}{};",
        relation.quoted_name(),
        if restart_identity { " RESTART IDENTITY" } else { "" },
        if cascade { " CASCADE" } else { "" }
    )
}

/// `REFRESH MATERIALIZED VIEW "schema"."name"`.
#[uniffi::export]
pub fn refresh_materialized_view_sql(relation: Relation) -> String {
    format!("REFRESH MATERIALIZED VIEW {};", relation.quoted_name())
}

/// `DROP FUNCTION|PROCEDURE "schema"."name"(arguments)`. The arguments pick
/// the one overload; their names, as pg_get_function_identity_arguments
/// prints them, are accepted there.
#[uniffi::export]
pub fn drop_routine_sql(routine: Routine, cascade: bool) -> String {
    format!(
        "DROP {} {}.{}({}){};",
        if routine.is_procedure { "PROCEDURE" } else { "FUNCTION" },
        quote_identifier(&routine.schema),
        quote_identifier(&routine.name),
        routine.arguments,
        if cascade { " CASCADE" } else { "" }
    )
}

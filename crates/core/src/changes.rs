use std::collections::HashMap;

use crate::rows::{quote_identifier, quote_literal};
use crate::{Column, DatabaseError, DatabaseSession, Engine, Relation};

/// A value for a cell. For a new row, a column with no draft gets its DEFAULT.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum CellDraft {
    /// Typed text; always sent as a quoted literal, never as SQL.
    Value { text: String },
    Null,
    /// The column's DEFAULT expression.
    Default,
    /// One of a fixed set of SQL functions, from the cell menu.
    Function { function: SqlFunction },
}

/// The only SQL that a cell can hold. A closed list, so typed text can't
/// become SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SqlFunction {
    Now,
    CurrentDate,
    CurrentTime,
}

impl SqlFunction {
    fn sql(self, engine: Engine) -> &'static str {
        match (self, engine) {
            (Self::Now, Engine::Mssql) => "SYSDATETIME()",
            (Self::CurrentDate, Engine::Mssql) => "CAST(SYSDATETIME() AS date)",
            (Self::CurrentTime, Engine::Mssql) => "CAST(SYSDATETIME() AS time)",
            (Self::Now, Engine::Cassandra) => "toTimestamp(now())",
            // Oracle has no time-only type: the current time is a date like the others.
            (Self::Now | Self::CurrentTime, Engine::Oracle) => "SYSDATE",
            (Self::CurrentDate, Engine::Oracle) => "TRUNC(SYSDATE)",
            (Self::CurrentDate, Engine::Cassandra) => "currentDate()",
            (Self::CurrentTime, Engine::Cassandra) => "currentTime()",
            (Self::Now, Engine::Postgres | Engine::Mysql | Engine::Redis | Engine::Mongo) => "now()",
            (Self::Now, Engine::Sqlite | Engine::Libsql) => "CURRENT_TIMESTAMP",
            (Self::CurrentDate, _) => "CURRENT_DATE",
            (Self::CurrentTime, _) => "CURRENT_TIME",
        }
    }
}

/// A row added with "+ Row", waiting for Commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct NewRow {
    /// By column name.
    pub values: HashMap<String, CellDraft>,
}

/// Changed cells of one existing row, found by its primary key.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RowUpdate {
    /// The primary key values, in primary key column order.
    pub key: Vec<String>,
    /// By column name.
    pub values: HashMap<String, CellDraft>,
}

/// Everything waiting for Commit in one table tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct PendingChanges {
    pub inserts: Vec<NewRow>,
    pub updates: Vec<RowUpdate>,
    /// Primary key values of the rows to delete.
    pub deletes: Vec<Vec<String>>,
}

/// `INSERT` for one new row. Values are untyped literals, so PostgreSQL reads
/// each as its column's type. `columns` gives the column order.
#[uniffi::export]
pub fn insert_sql(row: &NewRow, relation: &Relation, columns: &[Column], engine: Engine) -> Result<String, DatabaseError> {
    if engine == Engine::Mongo {
        return crate::mongo::insert_command(row, relation, columns);
    }
    // SQLite has no DEFAULT in VALUES: a column left out gets its default.
    let set: Vec<&Column> = columns
        .iter()
        .filter(|column| match row.values.get(&column.name) {
            Some(CellDraft::Default) => !engine.is_sqlite(),
            Some(_) => true,
            None => false,
        })
        .collect();
    if set.is_empty() {
        if engine == Engine::Mysql {
            return Ok(format!("INSERT INTO {} () VALUES ()", relation.quoted_name()));
        }
        // Oracle has no DEFAULT VALUES: one column gets its default, the rest follow.
        if let (Engine::Oracle, Some(first)) = (engine, columns.first()) {
            return Ok(format!("INSERT INTO {} ({}) VALUES (DEFAULT)", relation.quoted_name(), quote_identifier(&first.name)));
        }
        return Ok(format!("INSERT INTO {} DEFAULT VALUES", relation.quoted_name()));
    }
    let names = set.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ");
    let values = set
        .iter()
        .map(|column| draft_sql(row.values.get(&column.name), engine))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(format!("INSERT INTO {} ({names}) VALUES ({values})", relation.quoted_name()))
}

/// The statements for all pending changes, as the user reviews them:
/// updates, then deletes, then inserts.
#[uniffi::export]
pub fn changes_sql(
    changes: &PendingChanges,
    relation: &Relation,
    columns: &[Column],
    primary_key: &[String],
    engine: Engine,
) -> Result<Vec<String>, DatabaseError> {
    if engine == Engine::Mongo {
        return crate::mongo::change_commands(changes, relation, columns);
    }
    if engine == Engine::Cassandra {
        return crate::cassandra::change_statements(changes, relation, columns, primary_key);
    }
    let mut statements = Vec::new();
    for update in &changes.updates {
        let set: Vec<&Column> = columns.iter().filter(|column| update.values.contains_key(&column.name)).collect();
        if set.is_empty() {
            continue;
        }
        let assignments = set
            .iter()
            .map(|column| {
                let value = draft_sql(update.values.get(&column.name), engine)?;
                Ok(format!("{} = {value}", quote_identifier(&column.name)))
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?
            .join(", ");
        statements.push(format!(
            "UPDATE {} SET {assignments} WHERE {}",
            relation.quoted_name(),
            key_condition(primary_key, &update.key, engine)?
        ));
    }
    for key in &changes.deletes {
        statements.push(format!("DELETE FROM {} WHERE {}", relation.quoted_name(), key_condition(primary_key, key, engine)?));
    }
    for row in &changes.inserts {
        statements.push(insert_sql(row, relation, columns, engine)?);
    }
    Ok(statements)
}

#[uniffi::export]
impl DatabaseSession {
    /// Inserts the new rows atomically. See [`DatabaseSession::commit`].
    pub async fn insert(&self, rows: Vec<NewRow>, relation: Relation, columns: Vec<Column>) -> Result<(), DatabaseError> {
        let changes = PendingChanges { inserts: rows, ..Default::default() };
        self.commit(changes, relation, columns, Vec::new()).await
    }

    /// Applies all changes, or none of them. The statements run in one DO
    /// block, sent as one message: PostgreSQL runs it in one implicit
    /// transaction. An UPDATE or DELETE that finds no row (someone else
    /// changed or deleted it) raises an error, so nothing is applied.
    pub async fn commit(
        &self,
        changes: PendingChanges,
        relation: Relation,
        columns: Vec<Column>,
        primary_key: Vec<String>,
    ) -> Result<(), DatabaseError> {
        if self.settings().read_only {
            return Err(DatabaseError::new("This connection is read-only. Nothing was saved."));
        }
        if (!changes.updates.is_empty() || !changes.deletes.is_empty()) && primary_key.is_empty() {
            return Err(DatabaseError::new("Rows of a table without a primary key can't be changed."));
        }
        let engine = self.engine();
        if engine == Engine::Mongo {
            return self.mongo_commit(relation, changes, columns).await;
        }
        if engine == Engine::Cassandra {
            return self.cassandra_commit(changes, relation, columns, primary_key).await;
        }
        let statements = changes_sql(&changes, &relation, &columns, &primary_key, engine)?;
        if statements.is_empty() {
            return Ok(());
        }
        let checked = changes.updates.len() + changes.deletes.len();
        if let Some(link) = self.sqlite() {
            return crate::on_runtime(async move { link.apply_checked(statements, checked).await }).await;
        }
        if let Some(link) = self.mysql() {
            return crate::on_runtime(async move { link.apply_checked(statements, checked).await }).await;
        }
        if let Some(link) = self.mssql() {
            return crate::on_runtime(async move { link.apply_checked(statements, checked).await }).await;
        }
        if let Some(link) = self.oracle() {
            return crate::on_runtime(async move { link.apply_checked(statements, checked).await }).await;
        }
        self.execute(do_block(&statements, checked)?).await
    }
}

/// `$tag$ … $tag$` around the body, with a tag that no value contains.
fn do_block(statements: &[String], checked: usize) -> Result<String, DatabaseError> {
    let mut body = String::from("BEGIN\n");
    for (index, statement) in statements.iter().enumerate() {
        body += &format!("  {statement};\n");
        // Updates and deletes come first; each must hit its row.
        if index < checked {
            body += "  IF NOT FOUND THEN\n    RAISE EXCEPTION 'A row was changed or deleted by someone else. Nothing was saved; refresh and try again.';\n  END IF;\n";
        }
    }
    body += "END";
    let mut tag = String::from("tableport");
    while body.contains(&format!("${tag}$")) {
        tag += "_";
    }
    Ok(format!("DO ${tag}$\n{body}\n${tag}$"))
}

fn draft_sql(draft: Option<&CellDraft>, engine: Engine) -> Result<String, DatabaseError> {
    match draft {
        Some(CellDraft::Value { text }) => literal(text, engine),
        Some(CellDraft::Null) | None => Ok("NULL".to_string()),
        Some(CellDraft::Default) if engine.is_sqlite() => {
            Err(DatabaseError::new("SQLite can't set an existing value back to the column's default. Type the value instead."))
        }
        Some(CellDraft::Default) => Ok("DEFAULT".to_string()),
        Some(CellDraft::Function { function }) => Ok(function.sql(engine).to_string()),
    }
}

/// A text literal; N'…' on SQL Server, which would otherwise turn text
/// outside the database's code page into question marks.
fn literal(text: &str, engine: Engine) -> Result<String, DatabaseError> {
    match engine {
        Engine::Mssql => crate::mssql::literal_text(text),
        _ => quote_literal(text),
    }
}

fn key_condition(primary_key: &[String], key: &[String], engine: Engine) -> Result<String, DatabaseError> {
    if primary_key.is_empty() || primary_key.len() != key.len() {
        return Err(DatabaseError::new("The row's primary key doesn't match the table's."));
    }
    Ok(primary_key
        .iter()
        .zip(key)
        .map(|(column, value)| Ok(format!("{} = {}", quote_identifier(column), literal(value, engine)?)))
        .collect::<Result<Vec<_>, DatabaseError>>()?
        .join(" AND "))
}

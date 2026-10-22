//! Copy and export: selected rows as CSV or INSERT statements, and whole
//! tables (or their filtered rows) to CSV or JSON files.

use futures_util::{pin_mut, StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;
use tokio_postgres::SimpleQueryMessage;

use crate::rows::{quote_identifier, quote_literal, where_clause, Dialect};
use crate::{on_runtime, Column, DatabaseError, DatabaseSession, Relation, RowFilter, RowSort};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExportFormat {
    /// RFC 4180, with a header row; NULL is an empty field.
    Csv,
    /// One JSON array of objects, with the column types kept.
    Json,
    /// One INSERT statement per row, as Copy as SQL makes.
    Sql,
}

/// Which rows an export writes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ExportScope {
    /// Every row that matches the filters, in the sort order.
    All,
    /// One page, as the grid shows it.
    Page { offset: u64, limit: u64 },
    /// The rows with these primary keys (one value per key column, as text).
    Keys { keys: Vec<Vec<String>> },
}

#[uniffi::export]
impl DatabaseSession {
    /// Writes the rows to `path` and returns how many it wrote. The rows go
    /// from the server to the file in a stream, so a large table doesn't
    /// fill memory. CSV comes from PostgreSQL's own COPY; JSON from
    /// row_to_json, so numbers, booleans and json stay typed.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        primary_key: Vec<String>,
        scope: ExportScope,
        format: ExportFormat,
        path: String,
    ) -> Result<u64, DatabaseError> {
        let select = select_sql(&relation, &columns, &filters, sort.as_ref(), &primary_key, &scope, self.dialect())?;
        if format == ExportFormat::Sql && columns.is_empty() {
            return Err(DatabaseError::new("The table's columns are not loaded yet."));
        }
        if let Some(link) = self.sqlite() {
            return on_runtime(async move { link.export(select, columns, relation, format, path).await }).await;
        }
        if let Some(link) = self.mysql() {
            return on_runtime(async move { link.export(select, columns, relation, format, path).await }).await;
        }
        let client = self.client()?;
        let endpoint = self.settings().endpoint();
        on_runtime(async move {
            let map = |error| DatabaseError::from_postgres(error, &endpoint);
            let file = tokio::fs::File::create(&path)
                .await
                .map_err(|error| DatabaseError::new(format!("Can't create {path}: {error}.")))?;
            let mut file = tokio::io::BufWriter::new(file);
            let write_error = |error: std::io::Error| DatabaseError::new(format!("Can't write {path}: {error}."));
            let mut count = 0u64;
            match format {
                ExportFormat::Csv => {
                    let stream = client
                        .copy_out(&format!("COPY ({select}) TO STDOUT WITH (FORMAT csv, HEADER true)"))
                        .await
                        .map_err(map)?;
                    pin_mut!(stream);
                    let mut records = CsvRecordCounter::default();
                    while let Some(chunk) = stream.try_next().await.map_err(map)? {
                        records.feed(&chunk);
                        file.write_all(&chunk).await.map_err(write_error)?;
                    }
                    // The header is one record too.
                    count = records.count.saturating_sub(1);
                }
                ExportFormat::Json => {
                    let stream = client
                        .simple_query_raw(&format!("SELECT row_to_json(t)::text FROM ({select}) t"))
                        .await
                        .map_err(map)?;
                    pin_mut!(stream);
                    file.write_all(b"[").await.map_err(write_error)?;
                    while let Some(message) = stream.next().await {
                        if let SimpleQueryMessage::Row(row) = message.map_err(map)? {
                            let object = row.get(0).unwrap_or("null");
                            let separator: &[u8] = if count == 0 { b"\n  " } else { b",\n  " };
                            file.write_all(separator).await.map_err(write_error)?;
                            file.write_all(object.as_bytes()).await.map_err(write_error)?;
                            count += 1;
                        }
                    }
                    file.write_all(if count == 0 { b"]\n" } else { b"\n]\n" }).await.map_err(write_error)?;
                }
                ExportFormat::Sql => {
                    // Every value as text, quoted here; PostgreSQL casts it
                    // back to the column type on insert.
                    let names = columns.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ");
                    let as_text = columns
                        .iter()
                        .map(|column| format!("t.{}::text", quote_identifier(&column.name)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let stream = client.simple_query_raw(&format!("SELECT {as_text} FROM ({select}) t")).await.map_err(map)?;
                    pin_mut!(stream);
                    let target = relation.quoted_name();
                    while let Some(message) = stream.next().await {
                        if let SimpleQueryMessage::Row(row) = message.map_err(map)? {
                            let values = (0..row.len())
                                .map(|index| row.get(index).map(quote_literal).unwrap_or_else(|| Ok("NULL".to_string())))
                                .collect::<Result<Vec<_>, _>>()?
                                .join(", ");
                            file.write_all(format!("INSERT INTO {target} ({names}) VALUES ({values});\n").as_bytes())
                                .await
                                .map_err(write_error)?;
                            count += 1;
                        }
                    }
                }
            }
            file.flush().await.map_err(write_error)?;
            Ok(count)
        })
        .await
    }

    /// The rows with these primary keys, whole: the grid cuts long values,
    /// and a copy must not. Rows come back in the order of `keys`; a key
    /// that no longer matches a row is left out.
    pub async fn full_rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        primary_key: Vec<String>,
        keys: Vec<Vec<String>>,
    ) -> Result<Vec<Vec<Option<String>>>, DatabaseError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if primary_key.is_empty() {
            return Err(DatabaseError::new("The table has no primary key, so its rows can't be read again."));
        }
        let dialect = self.dialect();
        let key_list = primary_key.iter().map(|name| dialect.text(&quote_identifier(name))).collect::<Vec<_>>().join(", ");
        let value_list =
            columns.iter().map(|column| dialect.full_text(&quote_identifier(&column.name), &column.column_type)).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT {key_list}, {value_list} FROM {} WHERE {}",
            relation.quoted_name(),
            keys_condition(&primary_key, &keys, dialect)?
        );
        let rows = self.text_rows(sql).await?;
        let width = primary_key.len();
        let mut by_key: std::collections::HashMap<Vec<String>, Vec<Option<String>>> = rows
            .into_iter()
            .filter(|row| row.len() >= width)
            .map(|mut row| {
                let values = row.split_off(width);
                (row.into_iter().map(Option::unwrap_or_default).collect(), values)
            })
            .collect();
        Ok(keys.iter().filter_map(|key| by_key.remove(key)).collect())
    }
}

/// The SELECT an export runs: the real columns (not the grid's cut text),
/// the filters, the order, and the scope.
fn select_sql(
    relation: &Relation,
    columns: &[Column],
    filters: &[RowFilter],
    sort: Option<&RowSort>,
    primary_key: &[String],
    scope: &ExportScope,
    dialect: Dialect,
) -> Result<String, DatabaseError> {
    let select_list = if columns.is_empty() {
        "*".to_string()
    } else {
        columns.iter().map(|column| quote_identifier(&column.name)).collect::<Vec<_>>().join(", ")
    };
    let mut sql = format!("SELECT {select_list} FROM {}", relation.quoted_name());
    match scope {
        ExportScope::Keys { keys } => {
            if primary_key.is_empty() {
                return Err(DatabaseError::new("The table has no primary key, so selected rows can't be exported."));
            }
            sql += &format!(" WHERE {}", keys_condition(primary_key, keys, dialect)?);
        }
        ExportScope::All | ExportScope::Page { .. } => sql += &where_clause(filters, columns, dialect)?,
    }
    if let Some(sort) = sort {
        sql += &format!(" ORDER BY {} {}", quote_identifier(&sort.column), if sort.ascending { "ASC" } else { "DESC" });
    } else if !primary_key.is_empty() {
        let key = primary_key.iter().map(|name| quote_identifier(name)).collect::<Vec<_>>().join(", ");
        sql += &format!(" ORDER BY {key}");
    }
    if let ExportScope::Page { offset, limit } = scope {
        sql += &format!(" LIMIT {limit} OFFSET {offset}");
    }
    Ok(sql)
}

/// `(a::text, b::text) IN (('1', 'x'), …)`; compared as text, as the grid
/// holds the keys.
fn keys_condition(primary_key: &[String], keys: &[Vec<String>], dialect: Dialect) -> Result<String, DatabaseError> {
    if keys.is_empty() {
        return Ok("false".to_string());
    }
    let columns = primary_key.iter().map(|name| dialect.text(&quote_identifier(name))).collect::<Vec<_>>().join(", ");
    let tuples = keys
        .iter()
        .map(|key| {
            if key.len() != primary_key.len() {
                return Err(DatabaseError::new("A row key doesn't match the primary key."));
            }
            Ok(format!("({})", key.iter().map(|value| quote_literal(value)).collect::<Result<Vec<_>, _>>()?.join(", ")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("({columns}) IN ({})", tuples.join(", ")))
}

/// Counts CSV records in a byte stream: newlines outside quotes. A quote
/// inside a quoted field is doubled, which toggles twice and changes nothing.
#[derive(Default)]
struct CsvRecordCounter {
    in_quotes: bool,
    count: u64,
}

impl CsvRecordCounter {
    fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match byte {
                b'"' => self.in_quotes = !self.in_quotes,
                b'\n' if !self.in_quotes => self.count += 1,
                _ => {}
            }
        }
    }
}

/// Rows as CSV, for the clipboard: RFC 4180 quoting, NULL as an empty field,
/// lines ended with \n.
#[uniffi::export]
pub fn rows_to_csv(columns: Vec<String>, rows: Vec<Vec<Option<String>>>, header: bool) -> String {
    fn field(text: &str) -> String {
        if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_string()
        }
    }
    let mut out = String::new();
    if header {
        out += &columns.iter().map(|name| field(name)).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    for row in rows {
        out += &row.iter().map(|cell| cell.as_deref().map(field).unwrap_or_default()).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out
}

/// One INSERT statement per row, for the clipboard. Values are quoted
/// literals; PostgreSQL casts them to the column types on insert.
#[uniffi::export]
pub fn rows_to_insert_sql(relation: Relation, columns: Vec<String>, rows: Vec<Vec<Option<String>>>) -> Result<String, DatabaseError> {
    let names = columns.iter().map(|name| quote_identifier(name)).collect::<Vec<_>>().join(", ");
    let target = relation.quoted_name();
    let mut out = String::new();
    for row in rows {
        let values = row
            .iter()
            .map(|cell| cell.as_deref().map(quote_literal).unwrap_or_else(|| Ok("NULL".to_string())))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        out += &format!("INSERT INTO {target} ({names}) VALUES ({values});\n");
    }
    Ok(out)
}

/// Rows as a JSON array of objects, for query results that are only in
/// memory. Every value is a string or null: the result holds text only.
#[uniffi::export]
pub fn rows_to_json(columns: Vec<String>, rows: Vec<Vec<Option<String>>>) -> String {
    fn string(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out += "\\\"",
                '\\' => out += "\\\\",
                '\n' => out += "\\n",
                '\r' => out += "\\r",
                '\t' => out += "\\t",
                c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    let objects = rows
        .iter()
        .map(|row| {
            let fields = columns
                .iter()
                .zip(row)
                .map(|(name, cell)| format!("{}: {}", string(name), cell.as_deref().map(string).unwrap_or_else(|| "null".to_string())))
                .collect::<Vec<_>>()
                .join(", ");
            format!("  {{{fields}}}")
        })
        .collect::<Vec<_>>();
    if objects.is_empty() {
        "[]\n".to_string()
    } else {
        format!("[\n{}\n]\n", objects.join(",\n"))
    }
}

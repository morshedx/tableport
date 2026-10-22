use crate::{DatabaseError, DatabaseSession, Relation};

/// Cells longer than this are cut for the grid; the cell editor will load
/// the full value. Keeps a page of large jsonb rows from using gigabytes.
pub const GRID_CELL_LIMIT: u32 = 2000;

/// [`GRID_CELL_LIMIT`] for Swift, which UniFFI doesn't export constants to.
#[uniffi::export]
pub fn grid_cell_limit() -> u32 {
    GRID_CELL_LIMIT
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct Column {
    pub name: String,
    /// As `format_type` prints it, for example "character varying(255)".
    pub column_type: String,
    /// What the grid offers for the cell beyond typing: a date picker,
    /// TRUE/FALSE, enum values.
    pub kind: ColumnKind,
    /// The labels of an enum column, in their defined order; empty otherwise.
    pub enum_values: Vec<String>,
    /// Integers, numeric, floats and money: the grid right-aligns them, so
    /// digits line up.
    pub is_numeric: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ColumnKind {
    Date,
    Time,
    /// timestamp without time zone
    Timestamp,
    /// timestamp with time zone
    TimestampTz,
    Boolean,
    Enum,
    Other,
}

impl Column {
    /// A column of no special kind, for lists that only need names.
    pub fn plain(name: impl Into<String>, column_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            column_type: column_type.into(),
            kind: ColumnKind::Other,
            enum_values: Vec::new(),
            is_numeric: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum FilterOperator {
    Equals,
    NotEquals,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    GreaterThan,
    GreaterOrEqual,
    LessThan,
    LessOrEqual,
    /// Comma-separated values.
    In,
    NotIn,
    /// The value is a LIKE pattern as typed: % and _ are wildcards.
    Like,
    IsNull,
    IsNotNull,
}

/// What a filter row looks at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum FilterTarget {
    Column { name: String },
    /// Every column, compared as text; a row matches if any column does.
    AnyColumn,
    /// The value is a WHERE condition written by the user, used as is.
    RawSql,
}

/// One row of the filter bar. A page applies all its filters with AND.
#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct RowFilter {
    pub target: FilterTarget,
    pub operator: FilterOperator,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, uniffi::Record)]
pub struct RowSort {
    pub column: String,
    pub ascending: bool,
}

/// One page of rows. Cells are text, None for NULL.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RowPage {
    pub rows: Vec<Vec<Option<String>>>,
    pub offset: u64,
}

#[uniffi::export]
impl DatabaseSession {
    pub async fn columns(&self, relation: Relation) -> Result<Vec<Column>, DatabaseError> {
        if self.dialect() == Dialect::Sqlite {
            return self.sqlite_columns(&relation).await;
        }
        let rows = self
            .text_rows(format!(
                // Enum labels are joined with the unit separator, which a
                // label is very unlikely to contain.
                r"SELECT a.attname, format_type(a.atttypid, a.atttypmod), t.typname, t.typcategory,
                        (SELECT string_agg(e.enumlabel, E'' ORDER BY e.enumsortorder)
                         FROM pg_enum e WHERE e.enumtypid = t.oid)
                 FROM pg_attribute a
                 JOIN pg_type t ON t.oid = a.atttypid
                 WHERE a.attrelid = {} AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                relation.regclass()?
            ))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let [Some(name), Some(column_type), Some(type_name), Some(category), labels] =
                    <[Option<String>; 5]>::try_from(row).ok()?
                else {
                    return None;
                };
                let kind = match (type_name.as_str(), category.as_str()) {
                    ("date", _) => ColumnKind::Date,
                    ("time" | "timetz", _) => ColumnKind::Time,
                    ("timestamp", _) => ColumnKind::Timestamp,
                    ("timestamptz", _) => ColumnKind::TimestampTz,
                    ("bool", _) => ColumnKind::Boolean,
                    (_, "E") => ColumnKind::Enum,
                    _ => ColumnKind::Other,
                };
                let enum_values = labels.map(|labels| labels.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default();
                let is_numeric = category == "N";
                Some(Column { name, column_type, kind, enum_values, is_numeric })
            })
            .collect())
    }

    pub async fn rows(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
        sort: Option<RowSort>,
        primary_key: Vec<String>,
        offset: u64,
        limit: u64,
    ) -> Result<RowPage, DatabaseError> {
        let dialect = self.dialect();
        let select_list = if columns.is_empty() {
            "NULL".to_string()
        } else {
            columns.iter().map(|column| dialect.cell(&quote_identifier(&column.name))).collect::<Vec<_>>().join(", ")
        };
        let mut sql = format!("SELECT {select_list} FROM {}", relation.quoted_name());
        sql += &where_clause(&filters, &columns, dialect)?;
        if let Some(sort) = &sort {
            sql += &format!(" ORDER BY {} {}", quote_identifier(&sort.column), if sort.ascending { "ASC" } else { "DESC" });
        } else if !primary_key.is_empty() {
            // Without an order PostgreSQL returns rows as stored, which moves an
            // updated row to the end and makes pages overlap.
            let key = primary_key.iter().map(|column| quote_identifier(column)).collect::<Vec<_>>().join(", ");
            sql += &format!(" ORDER BY {key}");
        }
        sql += &format!(" LIMIT {limit} OFFSET {offset}");
        let rows = self.text_rows(sql).await?;
        Ok(RowPage { rows, offset })
    }

    /// The planner's row estimate, read from statistics without scanning:
    /// instant on any size. None for views and tables never analyzed. A
    /// partitioned table adds up its partitions.
    pub async fn estimated_row_count(&self, relation: Relation) -> Result<Option<u64>, DatabaseError> {
        // SQLite keeps no row estimate; a file database counts quickly.
        if self.dialect() == Dialect::Sqlite {
            return self.row_count(relation, Vec::new(), Vec::new()).await.map(Some);
        }
        let name = quote_literal(&relation.quoted_name())?;
        let sql = format!(
            "SELECT CASE WHEN c.relkind = 'p' THEN \
                 (SELECT sum(child.reltuples) FILTER (WHERE child.reltuples >= 0)::bigint \
                  FROM pg_inherits i JOIN pg_class child ON child.oid = i.inhrelid WHERE i.inhparent = c.oid) \
             ELSE c.reltuples::bigint END \
             FROM pg_class c WHERE c.oid = {name}::regclass"
        );
        let rows = self.text_rows(sql).await?;
        let estimate = rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_deref())
            .and_then(|text| text.parse::<i64>().ok());
        // -1 means never analyzed.
        Ok(estimate.filter(|count| *count >= 0).map(|count| count as u64))
    }

    /// `columns` are needed for "Any column" filters.
    pub async fn row_count(
        &self,
        relation: Relation,
        columns: Vec<Column>,
        filters: Vec<RowFilter>,
    ) -> Result<u64, DatabaseError> {
        let mut sql = format!("SELECT count(*) FROM {}", relation.quoted_name());
        sql += &where_clause(&filters, &columns, self.dialect())?;
        let rows = self.text_rows(sql).await?;
        rows.first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_deref())
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| DatabaseError::new("The server did not return a row count."))
    }
}

/// The SQL that differs between engines, for the queries the grid builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// The value as text.
    pub(crate) fn text(self, expression: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expression}::text"),
            Dialect::Sqlite => format!("CAST({expression} AS TEXT)"),
        }
    }

    /// A grid cell: the first GRID_CELL_LIMIT characters as text. SQLite
    /// blobs show as `\x…` hex, as PostgreSQL shows bytea.
    fn cell(self, column: &str) -> String {
        match self {
            Dialect::Postgres => format!("left({column}::text, {GRID_CELL_LIMIT})"),
            Dialect::Sqlite => format!(
                "CASE WHEN typeof({column}) = 'blob' THEN '\\x' || lower(hex({column})) \
                 ELSE substr(CAST({column} AS TEXT), 1, {GRID_CELL_LIMIT}) END"
            ),
        }
    }

    /// The whole value as text, for copies and full values: SQLite blobs
    /// as `\\x…` hex.
    pub(crate) fn full_text(self, column: &str) -> String {
        match self {
            Dialect::Postgres => format!("{column}::text"),
            Dialect::Sqlite => {
                format!("CASE WHEN typeof({column}) = 'blob' THEN '\\x' || lower(hex({column})) ELSE CAST({column} AS TEXT) END")
            }
        }
    }

    /// Case-insensitive match with `\` escaping `%` and `_`.
    fn contains(self, text: &str, pattern: &str, negated: bool) -> String {
        let not = if negated { "NOT " } else { "" };
        match self {
            Dialect::Postgres => format!("{text} {not}ILIKE {pattern}"),
            // SQLite's LIKE ignores case (for ASCII) and has no default escape.
            Dialect::Sqlite => format!("{text} {not}LIKE {pattern} ESCAPE '\\'"),
        }
    }

    fn is_distinct_from(self) -> &'static str {
        match self {
            Dialect::Postgres => "IS DISTINCT FROM",
            Dialect::Sqlite => "IS NOT",
        }
    }
}

/// `"name"`, with inner quotes doubled.
pub(crate) fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'text'`, with inner quotes doubled. Relies on standard_conforming_strings,
/// the default since PostgreSQL 9.1, so backslashes need no escaping.
pub(crate) fn quote_literal(text: &str) -> Result<String, DatabaseError> {
    if text.contains('\0') {
        return Err(DatabaseError::new("The value contains a NUL character."));
    }
    Ok(format!("'{}'", text.replace('\'', "''")))
}

/// " WHERE a AND b", or nothing without filters.
pub(crate) fn where_clause(filters: &[RowFilter], columns: &[Column], dialect: Dialect) -> Result<String, DatabaseError> {
    if filters.is_empty() {
        return Ok(String::new());
    }
    let conditions = filters.iter().map(|filter| filter.sql(columns, dialect)).collect::<Result<Vec<_>, _>>()?;
    Ok(format!(" WHERE {}", conditions.join(" AND ")))
}

impl RowFilter {
    /// The condition, in parentheses. Values are untyped literals, so for a
    /// column PostgreSQL reads them as the column's type: `id = '5'` works on
    /// an integer column and can use its index. "Any column" compares text,
    /// since one value can't fit every column's type.
    pub(crate) fn sql(&self, columns: &[Column], dialect: Dialect) -> Result<String, DatabaseError> {
        match &self.target {
            FilterTarget::RawSql => {
                let condition = self.value.trim().trim_end_matches(';').trim();
                if condition.is_empty() {
                    return Err(DatabaseError::new("The Raw SQL filter is empty."));
                }
                Ok(format!("({condition})"))
            }
            FilterTarget::Column { name } => {
                Ok(format!("({})", condition(&quote_identifier(name), false, self.operator, &self.value, dialect)?))
            }
            FilterTarget::AnyColumn => {
                if columns.is_empty() {
                    return Ok("(false)".into());
                }
                let any = columns
                    .iter()
                    .map(|column| condition(&quote_identifier(&column.name), true, self.operator, &self.value, dialect))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("({})", any.join(" OR ")))
            }
        }
    }
}

/// One comparison of `column` with `value`; `as_text` casts the column first.
fn condition(column: &str, as_text: bool, operator: FilterOperator, value: &str, dialect: Dialect) -> Result<String, DatabaseError> {
    let text = dialect.text(column);
    let typed = if as_text { text.clone() } else { column.to_string() };
    let pattern = |prefix: &str, suffix: &str| -> Result<String, DatabaseError> {
        let escaped = value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        quote_literal(&format!("{prefix}{escaped}{suffix}"))
    };
    let list = || -> Result<String, DatabaseError> {
        let items = value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(quote_literal)
            .collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Err(DatabaseError::new("IN needs at least one value, separated by commas."));
        }
        Ok(items.join(", "))
    };
    Ok(match operator {
        FilterOperator::Equals => format!("{typed} = {}", quote_literal(value)?),
        FilterOperator::NotEquals => format!("{typed} {} {}", dialect.is_distinct_from(), quote_literal(value)?),
        FilterOperator::GreaterThan => format!("{typed} > {}", quote_literal(value)?),
        FilterOperator::GreaterOrEqual => format!("{typed} >= {}", quote_literal(value)?),
        FilterOperator::LessThan => format!("{typed} < {}", quote_literal(value)?),
        FilterOperator::LessOrEqual => format!("{typed} <= {}", quote_literal(value)?),
        FilterOperator::Contains => dialect.contains(&text, &pattern("%", "%")?, false),
        FilterOperator::NotContains => format!("({column} IS NULL OR {})", dialect.contains(&text, &pattern("%", "%")?, true)),
        FilterOperator::StartsWith => dialect.contains(&text, &pattern("", "%")?, false),
        FilterOperator::EndsWith => dialect.contains(&text, &pattern("%", "")?, false),
        FilterOperator::Like => format!("{text} LIKE {}", quote_literal(value)?),
        FilterOperator::In => format!("{typed} IN ({})", list()?),
        FilterOperator::NotIn => format!("{typed} NOT IN ({})", list()?),
        FilterOperator::IsNull => format!("{column} IS NULL"),
        FilterOperator::IsNotNull => format!("{column} IS NOT NULL"),
    })
}

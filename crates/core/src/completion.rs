//! Autocomplete for the query editor. The catalog (relations, their
//! columns, functions) loads once per connection into a [`Completer`]; each
//! keystroke then asks it for suggestions without a round trip.

use std::collections::HashSet;
use std::sync::Arc;

use crate::schema::RelationKind;
use crate::{DatabaseError, DatabaseSession};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum CompletionKind {
    Table,
    View,
    Column,
    Function,
    Keyword,
    Schema,
}

/// How suggested keywords are written. Names keep their own case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum KeywordCase {
    /// `SELECT`, as Beautify writes them.
    Upper,
    Lower,
    /// Uppercase when the typed word is uppercase.
    AsTyped,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CompletionItem {
    /// What the list shows.
    pub label: String,
    /// What replaces the typed word: quoted or schema-qualified when needed.
    pub insert_text: String,
    /// Grey text at the right: the schema, the table a column comes from,
    /// or "keyword".
    pub detail: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Completions {
    /// UTF-16 offset where the typed word starts; the chosen item replaces
    /// the text from here to the cursor.
    pub replace_start: u32,
    pub items: Vec<CompletionItem>,
}

struct CatalogRelation {
    schema: String,
    name: String,
    kind: RelationKind,
    columns: Vec<String>,
}

/// The names one connection's editor can suggest.
#[derive(uniffi::Object)]
pub struct Completer {
    relations: Vec<CatalogRelation>,
    /// (schema, name), sorted by name; one per name and schema.
    functions: Vec<(String, String)>,
    schemas: Vec<String>,
    /// Schemas searched for unqualified names, in order.
    search_path: Vec<String>,
    /// MySQL quotes names with backticks; the others with double quotes.
    backticks: bool,
}

const MAX_ITEMS: usize = 100;

#[uniffi::export]
impl DatabaseSession {
    /// Reads the catalog for autocomplete: every relation with its columns,
    /// the functions, the schemas and the search path.
    pub async fn completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        match self.engine() {
            crate::Engine::Sqlite | crate::Engine::Libsql => return self.sqlite_completer().await,
            crate::Engine::Mysql => return self.mysql_completer().await,
            crate::Engine::Mssql => return self.mssql_completer().await,
            crate::Engine::Cassandra => return self.cassandra_completer().await,
            crate::Engine::Oracle => return self.oracle_completer().await,
            crate::Engine::Redis | crate::Engine::Mongo => return Err(DatabaseError::new("This connection has no SQL to complete.")),
            crate::Engine::Postgres => {}
        }
        let relation_rows = self
            .text_rows(
                r"SELECT n.nspname, c.relname, c.relkind,
                        (SELECT string_agg(a.attname, chr(31) ORDER BY a.attnum) FROM pg_attribute a
                         WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped)
                  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition
                    AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\_toast%'
                    AND n.nspname NOT LIKE 'pg\_temp\_%'
                  ORDER BY c.relname, n.nspname"
                    .into(),
            )
            .await?;
        let function_rows = self
            .text_rows(
                // Functions that take or return Postgres' internal types (type
                // input and output, planner estimates, handlers) can't be
                // called usefully from SQL: they only clutter the list.
                r"SELECT DISTINCT n.nspname, p.proname
                  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                  WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\_toast%'
                    AND p.proname NOT LIKE '\_%'
                    AND p.prorettype NOT IN ('internal'::regtype, 'cstring'::regtype, 'trigger'::regtype,
                        'event_trigger'::regtype, 'language_handler'::regtype, 'fdw_handler'::regtype,
                        'index_am_handler'::regtype, 'tsm_handler'::regtype, 'table_am_handler'::regtype)
                    AND NOT 'internal'::regtype = ANY (p.proargtypes::regtype[])
                    AND NOT 'cstring'::regtype = ANY (p.proargtypes::regtype[])
                  ORDER BY p.proname, n.nspname"
                    .into(),
            )
            .await?;
        let schema_rows = self
            .text_rows(
                r"SELECT nspname FROM pg_namespace
                  WHERE nspname NOT IN ('pg_catalog', 'information_schema')
                    AND nspname NOT LIKE 'pg\_toast%' AND nspname NOT LIKE 'pg\_temp\_%'
                  ORDER BY nspname"
                    .into(),
            )
            .await?;
        let path_rows = self.text_rows("SELECT unnest(current_schemas(true))".into()).await?;

        let relations = relation_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(schema), Some(name), Some(kind), columns] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                let kind = match kind.as_str() {
                    "r" => RelationKind::Table,
                    "p" => RelationKind::PartitionedTable,
                    "v" => RelationKind::View,
                    "m" => RelationKind::MaterializedView,
                    _ => RelationKind::ForeignTable,
                };
                let columns = columns.map(|text| text.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default();
                Some(CatalogRelation { schema, name, kind, columns })
            })
            .collect();
        let functions = function_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(schema), Some(name)] = <[Option<String>; 2]>::try_from(row).ok()? else { return None };
                Some((schema, name))
            })
            .collect();
        let first = |rows: Vec<Vec<Option<String>>>| rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect();
        Ok(Arc::new(Completer { relations, functions, schemas: first(schema_rows), search_path: first(path_rows), backticks: false }))
    }
}

#[uniffi::export]
impl Completer {
    /// Suggestions at `cursor` (UTF-16 offset). `explicit` is a request from
    /// the user (Esc): then an empty word gets suggestions too; while typing,
    /// only a word or a `name.` does.
    pub fn complete(&self, sql: String, cursor: u32, explicit: bool, keyword_case: KeywordCase) -> Completions {
        let cursor = byte_offset(&sql, cursor as usize);
        let none = Completions { replace_start: utf16_offset(&sql, cursor), items: Vec::new() };
        let tokens = tokenize(&sql);
        // Inside a string or a comment: nothing to suggest.
        if tokens.iter().any(|token| token.opaque && token.start < cursor && (cursor < token.end || !token.closed)) {
            return none;
        }

        // The statement around the cursor, split at semicolons.
        let start = tokens.iter().rposition(|t| t.end <= cursor && t.is_punct(';')).map_or(0, |index| index + 1);
        let end = tokens[start..].iter().position(|t| t.start >= cursor && t.is_punct(';')).map_or(tokens.len(), |index| start + index);
        let statement: Vec<&Token> = tokens[start..end].iter().filter(|t| !t.opaque).collect();

        // The word being typed, if the cursor is at the end of one.
        let word = statement.iter().position(|t| t.kind == Kind::Word && t.start < cursor && cursor <= t.end);
        let (prefix, replace_start) = match word {
            Some(index) => (&sql[statement[index].start..cursor], statement[index].start),
            None => ("", cursor),
        };
        let before: Vec<&Token> = statement.iter().copied().filter(|t| t.end <= replace_start).collect();
        let others: Vec<&Token> = statement.iter().enumerate().filter(|(index, _)| Some(*index) != word).map(|(_, t)| *t).collect();
        let scope = Scope::read(&others, &sql, self);

        let qualifier = match before.as_slice() {
            [.., name, dot] if dot.is_punct('.') && name.is_name() => Some(name.name(&sql)),
            _ => None,
        };
        if prefix.is_empty() && qualifier.is_none() && !explicit {
            return none;
        }

        let mut list = List::new(prefix, keyword_case);
        if let Some(qualifier) = qualifier {
            // `alias.` or `table.`: its columns; `schema.`: its relations.
            // A name still being typed (`FROM billing.`) reads as a relation
            // with no columns; it must not hide a schema of that name.
            if let Some(entry) = scope.find(&qualifier).filter(|entry| !entry.columns.is_empty()) {
                for column in &entry.columns {
                    list.push(column, self.quote(column), &entry.label, CompletionKind::Column);
                }
            } else if let Some(relation) = self.relation(None, &qualifier) {
                for column in &relation.columns {
                    list.push(column, self.quote(column), &relation.name, CompletionKind::Column);
                }
            } else if self.schemas.iter().any(|schema| schema.eq_ignore_ascii_case(&qualifier)) {
                for relation in self.relations.iter().filter(|r| r.schema.eq_ignore_ascii_case(&qualifier)) {
                    list.push(&relation.name, self.quote(&relation.name), &relation.schema, relation_kind(relation.kind));
                }
                for (schema, name) in self.functions.iter().filter(|(schema, _)| schema.eq_ignore_ascii_case(&qualifier)) {
                    list.push(name, self.quote(name), schema, CompletionKind::Function);
                }
            }
            return Completions { replace_start: utf16_offset(&sql, replace_start), items: list.finish() };
        }

        match context(&before, &sql) {
            Context::Start => list.keywords(STATEMENT_KEYWORDS),
            Context::Relations => {
                for cte in &scope.ctes {
                    list.push(&cte.label, self.quote(&cte.label), "CTE", CompletionKind::Table);
                }
                self.push_relations(&mut list);
                for schema in &self.schemas {
                    list.push(schema, self.quote(schema), "schema", CompletionKind::Schema);
                }
                list.keywords(KEYWORDS);
            }
            Context::Columns => {
                let mut seen = HashSet::new();
                for entry in &scope.entries {
                    for column in &entry.columns {
                        if seen.insert(column.as_str()) {
                            list.push(column, self.quote(column), &entry.label, CompletionKind::Column);
                        }
                    }
                }
                // The statement's tables and aliases, to qualify a column:
                // `cu` → `customers`, then `.` lists its columns.
                for entry in &scope.entries {
                    let detail = entry.names.first().filter(|name| **name != entry.label).map_or("table", String::as_str);
                    list.push(&entry.label, self.quote(&entry.label), detail, CompletionKind::Table);
                }
                // No FROM yet (`SELECT * c`): the name is likely a table.
                if scope.entries.is_empty() {
                    self.push_relations(&mut list);
                }
                list.keywords(KEYWORDS);
                self.push_functions(&mut list);
            }
            Context::Any => {
                list.keywords(KEYWORDS);
                self.push_relations(&mut list);
                self.push_functions(&mut list);
            }
        }
        Completions { replace_start: utf16_offset(&sql, replace_start), items: list.finish() }
    }
}

impl DatabaseSession {
    /// SQLite's catalog: the tables and views of every attached database
    /// with their columns, and SQLite's built-in functions. `main` and
    /// `temp` are searched for unqualified names, as SQLite does.
    async fn sqlite_completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        let schemas = self.schemas().await?;
        let mut relations = Vec::new();
        for schema in &schemas {
            for relation in &schema.relations {
                let columns = self.columns(relation.clone()).await?.into_iter().map(|column| column.name).collect();
                relations.push(CatalogRelation {
                    schema: relation.schema.clone(),
                    name: relation.name.clone(),
                    kind: relation.kind,
                    columns,
                });
            }
        }
        relations.sort_by(|a, b| a.name.cmp(&b.name));
        let functions = SQLITE_FUNCTIONS.iter().map(|name| ("main".to_string(), name.to_string())).collect();
        Ok(Arc::new(Completer {
            relations,
            functions,
            schemas: schemas.into_iter().map(|schema| schema.name).collect(),
            search_path: vec!["main".into(), "temp".into()],
            backticks: false,
        }))
    }

    async fn mysql_completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        let system = "'mysql', 'information_schema', 'performance_schema', 'sys'";
        let relation_rows = self
            .text_rows(format!(
                "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, t.TABLE_TYPE, \
                        (SELECT GROUP_CONCAT(c.COLUMN_NAME ORDER BY c.ORDINAL_POSITION SEPARATOR '\u{1f}') \
                         FROM information_schema.COLUMNS c WHERE c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME) \
                 FROM information_schema.TABLES t \
                 WHERE t.TABLE_SCHEMA NOT IN ({system}) AND t.TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
                 ORDER BY t.TABLE_NAME, t.TABLE_SCHEMA"
            ))
            .await?;
        let relations = relation_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(schema), Some(name), Some(kind), columns] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                let kind = if kind == "VIEW" { RelationKind::View } else { RelationKind::Table };
                let columns = columns.map(|columns| columns.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default();
                Some(CatalogRelation { schema, name, kind, columns })
            })
            .collect();
        let routine_rows = self
            .text_rows(format!(
                "SELECT ROUTINE_SCHEMA, ROUTINE_NAME FROM information_schema.ROUTINES \
                 WHERE ROUTINE_TYPE = 'FUNCTION' AND ROUTINE_SCHEMA NOT IN ({system})"
            ))
            .await?;
        let mut functions: Vec<(String, String)> = MYSQL_FUNCTIONS.iter().map(|name| (String::new(), name.to_string())).collect();
        functions.extend(routine_rows.into_iter().filter_map(|row| {
            let [Some(schema), Some(name)] = <[Option<String>; 2]>::try_from(row).ok()? else { return None };
            Some((schema, name))
        }));
        functions.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        functions.dedup();
        let schemas: Vec<String> = self.schemas().await?.into_iter().map(|schema| schema.name).collect();
        let current = self.text_rows("SELECT DATABASE()".into()).await?;
        // Built-in functions belong to no database; the empty schema finds them.
        let mut search_path = vec![String::new()];
        search_path.extend(current.into_iter().filter_map(|row| row.into_iter().next().flatten()));
        Ok(Arc::new(Completer { relations, functions, schemas, search_path, backticks: true }))
    }
}

impl DatabaseSession {
    /// SQL Server's catalog: the current database's tables and views with
    /// their columns. The user's default schema is searched for unqualified
    /// names, then dbo, as SQL Server does.
    async fn mssql_completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        let relation_rows = self
            .text_rows(
                "SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, \
                        (SELECT STRING_AGG(CAST(c.name AS nvarchar(max)), NCHAR(31)) WITHIN GROUP (ORDER BY c.column_id) \
                         FROM sys.columns c WHERE c.object_id = o.object_id) \
                 FROM sys.objects o WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY o.name"
                    .into(),
            )
            .await?;
        let relations = relation_rows
            .into_iter()
            .filter_map(|row| {
                let [Some(schema), Some(name), Some(kind), columns] = <[Option<String>; 4]>::try_from(row).ok()? else {
                    return None;
                };
                let kind = if kind.trim() == "V" { RelationKind::View } else { RelationKind::Table };
                let columns = columns.map(|columns| columns.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default();
                Some(CatalogRelation { schema, name, kind, columns })
            })
            .collect();
        let routine_rows = self
            .text_rows("SELECT SCHEMA_NAME(schema_id), name FROM sys.objects WHERE type IN ('FN', 'IF', 'TF') AND is_ms_shipped = 0".into())
            .await?;
        let mut functions: Vec<(String, String)> =
            crate::mssql::FUNCTIONS.iter().map(|name| (String::new(), name.to_string())).collect();
        functions.extend(routine_rows.into_iter().filter_map(|row| {
            let [Some(schema), Some(name)] = <[Option<String>; 2]>::try_from(row).ok()? else { return None };
            Some((schema, name))
        }));
        functions.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        functions.dedup();
        let schemas: Vec<String> = self.schemas().await?.into_iter().map(|schema| schema.name).collect();
        let current = self.text_rows("SELECT SCHEMA_NAME()".into()).await?;
        // Built-in functions belong to no schema; the empty schema finds them.
        let mut search_path = vec![String::new()];
        search_path.extend(current.into_iter().filter_map(|row| row.into_iter().next().flatten()));
        if !search_path.iter().any(|schema| schema == "dbo") {
            search_path.push("dbo".into());
        }
        Ok(Arc::new(Completer { relations, functions, schemas, search_path, backticks: false }))
    }
}

impl DatabaseSession {
    /// Every keyspace's tables and views with their columns. The keyspace
    /// the session started in is searched for unqualified names.
    async fn cassandra_completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        let schemas = self.schemas().await?;
        let rows = self.text_rows("SELECT keyspace_name, table_name, column_name FROM system_schema.columns".into()).await?;
        let mut columns: std::collections::HashMap<(String, String), Vec<String>> = std::collections::HashMap::new();
        for row in rows {
            let [Some(schema), Some(table), Some(column)] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else { continue };
            columns.entry((schema, table)).or_default().push(column);
        }
        let mut relations: Vec<CatalogRelation> = schemas
            .iter()
            .flat_map(|schema| schema.relations.iter())
            .map(|relation| CatalogRelation {
                schema: relation.schema.clone(),
                name: relation.name.clone(),
                kind: relation.kind,
                columns: columns.remove(&(relation.schema.clone(), relation.name.clone())).unwrap_or_default(),
            })
            .collect();
        relations.sort_by(|a, b| a.name.cmp(&b.name));
        let functions = crate::cassandra::FUNCTIONS.iter().map(|name| (String::new(), name.to_string())).collect();
        let mut search_path = vec![String::new()];
        search_path.extend(Some(self.settings().database.trim().to_string()).filter(|keyspace| !keyspace.is_empty()));
        Ok(Arc::new(Completer {
            relations,
            functions,
            schemas: schemas.into_iter().map(|schema| schema.name).collect(),
            search_path,
            backticks: false,
        }))
    }
}

impl DatabaseSession {
    /// The visible schemas' tables and views with their columns. The
    /// login's own schema is searched for unqualified names.
    async fn oracle_completer(&self) -> Result<Arc<Completer>, DatabaseError> {
        let schemas = self.schemas().await?;
        let owners = schemas.iter().map(|schema| crate::rows::quote_literal(&schema.name)).collect::<Result<Vec<_>, _>>()?;
        let mut columns: std::collections::HashMap<(String, String), Vec<String>> = std::collections::HashMap::new();
        if !owners.is_empty() {
            let rows = self
                .text_rows(format!(
                    "SELECT owner, table_name, column_name FROM all_tab_columns WHERE owner IN ({}) ORDER BY owner, table_name, column_id",
                    owners.join(", ")
                ))
                .await?;
            for row in rows {
                let [Some(schema), Some(table), Some(column)] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else { continue };
                columns.entry((schema, table)).or_default().push(column);
            }
        }
        let mut relations: Vec<CatalogRelation> = schemas
            .iter()
            .flat_map(|schema| schema.relations.iter())
            .map(|relation| CatalogRelation {
                schema: relation.schema.clone(),
                name: relation.name.clone(),
                kind: relation.kind,
                columns: columns.remove(&(relation.schema.clone(), relation.name.clone())).unwrap_or_default(),
            })
            .collect();
        relations.sort_by(|a, b| a.name.cmp(&b.name));
        let functions = crate::oracle::FUNCTIONS.iter().map(|name| (String::new(), name.to_string())).collect();
        let mut search_path = vec![String::new()];
        search_path.extend(self.oracle().map(|link| link.user));
        Ok(Arc::new(Completer {
            relations,
            functions,
            schemas: schemas.into_iter().map(|schema| schema.name).collect(),
            search_path,
            backticks: false,
        }))
    }
}

/// MySQL's and MariaDB's common built-in functions.
const MYSQL_FUNCTIONS: &[&str] = &[
    "abs", "acos", "adddate", "addtime", "aes_decrypt", "aes_encrypt", "any_value", "ascii", "asin", "atan", "atan2", "avg",
    "benchmark", "bin", "bit_and", "bit_count", "bit_length", "bit_or", "bit_xor", "cast", "ceil", "ceiling", "char",
    "char_length", "character_length", "charset", "coalesce", "collation", "concat", "concat_ws", "connection_id", "conv",
    "convert", "convert_tz", "cos", "cot", "count", "crc32", "cume_dist", "curdate", "current_date", "current_time",
    "current_timestamp", "current_user", "curtime", "database", "date", "date_add", "date_format", "date_sub", "datediff",
    "day", "dayname", "dayofmonth", "dayofweek", "dayofyear", "degrees", "dense_rank", "elt", "exp", "export_set",
    "extract", "field", "find_in_set", "first_value", "floor", "format", "found_rows", "from_base64", "from_days",
    "from_unixtime", "greatest", "group_concat", "hex", "hour", "if", "ifnull", "inet_aton", "inet_ntoa", "insert",
    "instr", "is_ipv4", "is_ipv6", "isnull", "json_array", "json_arrayagg", "json_contains", "json_contains_path",
    "json_depth", "json_extract", "json_insert", "json_keys", "json_length", "json_merge_patch", "json_object",
    "json_objectagg", "json_overlaps", "json_quote", "json_remove", "json_replace", "json_search", "json_set",
    "json_type", "json_unquote", "json_valid", "lag", "last_day", "last_insert_id", "last_value", "lcase", "lead",
    "least", "left", "length", "ln", "localtime", "localtimestamp", "locate", "log", "log10", "log2", "lower", "lpad",
    "ltrim", "make_set", "makedate", "maketime", "max", "md5", "microsecond", "mid", "min", "minute", "mod", "month",
    "monthname", "now", "nth_value", "ntile", "nullif", "oct", "octet_length", "ord", "percent_rank", "period_add",
    "period_diff", "pi", "position", "pow", "power", "quarter", "quote", "radians", "rand", "rank", "regexp_instr",
    "regexp_like", "regexp_replace", "regexp_substr", "release_lock", "repeat", "replace", "reverse", "right", "round",
    "row_count", "row_number", "rpad", "rtrim", "schema", "sec_to_time", "second", "session_user", "sha1", "sha2",
    "sign", "sin", "sleep", "soundex", "space", "sqrt", "std", "stddev", "stddev_pop", "stddev_samp", "str_to_date",
    "strcmp", "subdate", "substr", "substring", "substring_index", "subtime", "sum", "sysdate", "system_user", "tan",
    "time", "time_format", "time_to_sec", "timediff", "timestamp", "timestampadd", "timestampdiff", "to_base64",
    "to_days", "to_seconds", "trim", "truncate", "ucase", "unhex", "unix_timestamp", "upper", "user", "utc_date",
    "utc_time", "utc_timestamp", "uuid", "uuid_short", "var_pop", "var_samp", "variance", "version", "week",
    "weekday", "weekofyear", "year", "yearweek",
];

/// SQLite's built-in scalar, aggregate, date, math and JSON functions.
const SQLITE_FUNCTIONS: &[&str] = &[
    "abs", "avg", "changes", "char", "coalesce", "concat", "concat_ws", "count", "date", "datetime", "format",
    "glob", "group_concat", "hex", "ifnull", "iif", "instr", "json", "json_array", "json_array_length",
    "json_each", "json_extract", "json_group_array", "json_group_object", "json_insert", "json_object",
    "json_patch", "json_remove", "json_replace", "json_set", "json_tree", "json_type", "json_valid",
    "julianday", "last_insert_rowid", "length", "like", "likelihood", "lower", "ltrim", "max", "min", "nullif",
    "octet_length", "printf", "quote", "random", "randomblob", "replace", "round", "rtrim", "sign", "soundex",
    "sqlite_version", "strftime", "string_agg", "substr", "substring", "sum", "time", "timediff", "total",
    "total_changes", "trim", "typeof", "unhex", "unicode", "unixepoch", "upper", "zeroblob",
    "acos", "asin", "atan", "atan2", "ceil", "ceiling", "cos", "degrees", "exp", "floor", "ln", "log",
    "log10", "log2", "mod", "pi", "pow", "power", "radians", "sin", "sqrt", "tan", "trunc",
    "row_number", "rank", "dense_rank", "percent_rank", "cume_dist", "ntile", "lag", "lead",
    "first_value", "last_value", "nth_value",
];

impl Completer {
    /// A relation by name, from the search path first; `schema` qualifies it.
    fn relation(&self, schema: Option<&str>, name: &str) -> Option<&CatalogRelation> {
        let matches = self.relations.iter().filter(|r| r.name.eq_ignore_ascii_case(name));
        match schema {
            Some(schema) => matches.into_iter().find(|r| r.schema.eq_ignore_ascii_case(schema)),
            None => {
                let candidates: Vec<_> = matches.collect();
                self.search_path
                    .iter()
                    .find_map(|path| candidates.iter().find(|r| &r.schema == path).copied())
                    .or_else(|| candidates.first().copied())
            }
        }
    }

    fn on_path(&self, schema: &str) -> bool {
        self.search_path.iter().any(|path| path == schema)
    }

    /// Relations on the search path first; others insert as `schema.name`.
    fn push_relations(&self, list: &mut List) {
        for pass in [true, false] {
            for relation in self.relations.iter().filter(|r| self.on_path(&r.schema) == pass) {
                // System catalogs only when asked for by their prefix.
                if relation.schema == "pg_catalog" && !list.lower_prefix.starts_with("pg_") {
                    continue;
                }
                let insert = if pass {
                    self.quote(&relation.name)
                } else {
                    format!("{}.{}", self.quote(&relation.schema), self.quote(&relation.name))
                };
                list.push(&relation.name, insert, &relation.schema, relation_kind(relation.kind));
            }
        }
    }

    /// One entry per function name: the user's own schemas before pg_catalog.
    fn push_functions(&self, list: &mut List) {
        let mut seen = HashSet::new();
        for pass in [false, true] {
            for (schema, name) in self.functions.iter().filter(|(schema, _)| (schema == "pg_catalog") == pass) {
                if self.on_path(schema) && seen.insert(name.as_str()) {
                    list.push(name, self.quote(name), schema, CompletionKind::Function);
                }
            }
        }
    }
}

/// Quick open (⌘P): the indexes of `names` that match `query`, best first.
/// A prefix beats a word start (`it` in order_items), which beats any
/// substring, which beats letters in order with gaps; shorter names win a
/// tie. Case is ignored. An empty query keeps every name, in order.
#[uniffi::export]
pub fn rank_names(query: String, names: Vec<String>) -> Vec<u32> {
    let query = query.to_lowercase();
    let mut hits: Vec<(usize, usize, u32)> = names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let name = name.to_lowercase();
            let score = name_score(&name, &query)?;
            Some((score, name.chars().count(), index as u32))
        })
        .collect();
    if !query.is_empty() {
        hits.sort();
    }
    hits.into_iter().map(|(_, _, index)| index).collect()
}

/// Lower is better; None for no match. Both are lowercase.
fn name_score(name: &str, query: &str) -> Option<usize> {
    if query.is_empty() || name.starts_with(query) {
        return Some(0);
    }
    if let Some(position) = name.find(query) {
        let at_word_start = name[..position].ends_with(['_', '.', ' ', '-']);
        return Some(if at_word_start { 1 } else { 2 });
    }
    // Letters in order: fewer skipped characters rank higher.
    let mut name_chars = name.chars();
    let mut skipped = 0;
    for wanted in query.chars() {
        loop {
            let c = name_chars.next()?;
            if c == wanted {
                break;
            }
            skipped += 1;
        }
    }
    Some(3 + skipped)
}

/// 0 when `name` starts with `typed`. Otherwise, when the first letters
/// agree and the other typed letters appear in `name` in order (typing
/// `sl` for select, `cnt` for count): 1 plus where the last one was found,
/// so tighter matches rank higher. Both are lowercase.
fn match_score(name: &str, typed: &str) -> Option<usize> {
    if name.starts_with(typed) {
        return Some(0);
    }
    let mut typed_chars = typed.chars();
    let first = typed_chars.next()?;
    let mut name_chars = name.char_indices();
    if name_chars.next()?.1 != first || typed.chars().count() < 2 {
        return None;
    }
    let mut last = 0;
    for wanted in typed_chars {
        let (index, _) = name_chars.find(|(_, c)| *c == wanted)?;
        last = index;
    }
    Some(1 + last)
}

fn relation_kind(kind: RelationKind) -> CompletionKind {
    match kind {
        RelationKind::View | RelationKind::MaterializedView => CompletionKind::View,
        _ => CompletionKind::Table,
    }
}

/// The suggestions so far: names that start with the typed word, then
/// names that contain its letters in order (`sl` → select).
struct List {
    lower_prefix: String,
    upper: bool,
    /// With their match score: 0 for a prefix match, more for looser ones.
    items: Vec<(usize, CompletionItem)>,
    labels: HashSet<(String, CompletionKind)>,
}

impl List {
    fn new(prefix: &str, keyword_case: KeywordCase) -> Self {
        let upper = match keyword_case {
            KeywordCase::Upper => true,
            KeywordCase::Lower => false,
            KeywordCase::AsTyped => {
                !prefix.is_empty() && prefix.chars().all(|c| !c.is_ascii_lowercase()) && prefix.chars().any(|c| c.is_ascii_uppercase())
            }
        };
        Self { lower_prefix: prefix.to_lowercase(), upper, items: Vec::new(), labels: HashSet::new() }
    }

    fn push(&mut self, label: &str, insert_text: String, detail: &str, kind: CompletionKind) {
        let Some(score) = match_score(&label.to_lowercase(), &self.lower_prefix) else { return };
        if !self.labels.insert((label.to_string(), kind)) {
            return;
        }
        self.items.push((score, CompletionItem { label: label.to_string(), insert_text, detail: detail.to_string(), kind }));
    }

    /// Prefix matches first, in the order the context added them; then the
    /// looser matches, tightest first.
    fn finish(mut self) -> Vec<CompletionItem> {
        self.items.sort_by_key(|(score, _)| *score);
        self.items.into_iter().take(MAX_ITEMS).map(|(_, item)| item).collect()
    }

    fn keywords(&mut self, keywords: &[&str]) {
        // A keyword already typed in full needs no suggestion.
        for keyword in keywords {
            if *keyword == self.lower_prefix {
                continue;
            }
            let text = if self.upper { keyword.to_uppercase() } else { keyword.to_string() };
            self.push(keyword, text, "keyword", CompletionKind::Keyword);
        }
    }
}

enum Context {
    /// The start of a statement.
    Start,
    /// After FROM, JOIN, UPDATE, INTO.
    Relations,
    /// In a select list, a condition, ORDER BY, SET.
    Columns,
    Any,
}

fn context(before: &[&Token], sql: &str) -> Context {
    let Some(last) = before.last() else { return Context::Start };
    if last.is_punct('(') {
        return Context::Columns;
    }
    // The nearest clause keyword at this depth decides.
    let mut depth = 0;
    for token in before.iter().rev() {
        if token.is_punct(')') {
            depth += 1;
        } else if token.is_punct('(') {
            if depth == 0 {
                return Context::Columns;
            }
            depth -= 1;
        } else if depth == 0 && token.kind == Kind::Word {
            match token.keyword(sql).as_str() {
                "from" | "join" | "update" | "into" | "table" | "truncate" => {
                    // `FROM users u WHERE`-less: a word after the relation is
                    // its alias or the next clause, not another relation.
                    return if std::ptr::eq(*token, *last) || last.is_punct(',') { Context::Relations } else { Context::Any };
                }
                "select" | "where" | "on" | "and" | "or" | "by" | "set" | "having" | "returning" | "when" | "then"
                | "else" | "using" | "not" | "distinct" => return Context::Columns,
                _ => {}
            }
        }
    }
    Context::Any
}

/// The names a statement can refer to: relations in FROM and JOIN with their
/// aliases, CTEs, and subqueries with an alias.
struct Scope {
    entries: Vec<ScopeEntry>,
    ctes: Vec<ScopeEntry>,
}

struct ScopeEntry {
    /// The alias, else the relation's name.
    label: String,
    /// Other names that refer to it: the relation's own name.
    names: Vec<String>,
    columns: Vec<String>,
}

impl Scope {
    fn read(tokens: &[&Token], sql: &str, completer: &Completer) -> Self {
        let mut scope = Scope { entries: Vec::new(), ctes: Vec::new() };
        let mut index = 0;
        while index < tokens.len() {
            let keyword = if tokens[index].kind == Kind::Word { tokens[index].keyword(sql) } else { String::new() };
            match keyword.as_str() {
                "with" => index = scope.read_ctes(tokens, index + 1, sql),
                "from" | "join" | "update" | "into" => index = scope.read_relations(tokens, index + 1, sql, completer, keyword == "from"),
                _ => index += 1,
            }
        }
        scope
    }

    fn find(&self, name: &str) -> Option<&ScopeEntry> {
        let matches = |entry: &&ScopeEntry| {
            entry.label.eq_ignore_ascii_case(name) || entry.names.iter().any(|other| other.eq_ignore_ascii_case(name))
        };
        self.entries.iter().find(matches).or_else(|| self.ctes.iter().find(matches))
    }

    /// `name [(columns)] AS [NOT] [MATERIALIZED] (query)`, separated by commas.
    fn read_ctes(&mut self, tokens: &[&Token], mut index: usize, sql: &str) -> usize {
        if tokens.get(index).is_some_and(|t| t.keyword(sql) == "recursive") {
            index += 1;
        }
        loop {
            let Some(name) = tokens.get(index).filter(|t| t.is_name()) else { return index };
            let label = name.name(sql);
            index += 1;
            let mut columns = Vec::new();
            if tokens.get(index).is_some_and(|t| t.is_punct('(')) {
                let close = matching(tokens, index);
                columns = tokens[index + 1..close].iter().filter(|t| t.is_name()).map(|t| t.name(sql)).collect();
                index = close + 1;
            }
            while tokens.get(index).is_some_and(|t| matches!(t.keyword(sql).as_str(), "as" | "not" | "materialized")) {
                index += 1;
            }
            if tokens.get(index).is_some_and(|t| t.is_punct('(')) {
                let close = matching(tokens, index);
                if columns.is_empty() {
                    columns = select_list(&tokens[index + 1..close], sql);
                }
                index = close + 1;
            }
            self.ctes.push(ScopeEntry { label, names: Vec::new(), columns });
            if tokens.get(index).is_some_and(|t| t.is_punct(',')) {
                index += 1;
            } else {
                return index;
            }
        }
    }

    /// `[schema.]name [[AS] alias]` or `(query) [AS] alias`; after FROM, a
    /// comma starts another.
    fn read_relations(&mut self, tokens: &[&Token], mut index: usize, sql: &str, completer: &Completer, list: bool) -> usize {
        loop {
            while tokens.get(index).is_some_and(|t| matches!(t.keyword(sql).as_str(), "only" | "lateral")) {
                index += 1;
            }
            let Some(first) = tokens.get(index) else { return index };
            let (label, names, columns) = if first.is_punct('(') {
                let close = matching(tokens, index);
                let columns = select_list(&tokens[index + 1..close], sql);
                index = close + 1;
                (String::new(), Vec::new(), columns)
            } else if first.is_name() {
                let mut schema = None;
                let mut name = first.name(sql);
                index += 1;
                if tokens.get(index).is_some_and(|t| t.is_punct('.')) && tokens.get(index + 1).is_some_and(|t| t.is_name()) {
                    schema = Some(name);
                    name = tokens[index + 1].name(sql);
                    index += 2;
                }
                let columns = self
                    .ctes
                    .iter()
                    .find(|cte| schema.is_none() && cte.label.eq_ignore_ascii_case(&name))
                    .map(|cte| cte.columns.clone())
                    .or_else(|| completer.relation(schema.as_deref(), &name).map(|r| r.columns.clone()))
                    .unwrap_or_default();
                (name.clone(), vec![name], columns)
            } else {
                return index;
            };
            if tokens.get(index).is_some_and(|t| t.keyword(sql) == "as") {
                index += 1;
            }
            let mut label = label;
            if let Some(alias) = tokens.get(index).filter(|t| t.is_name() && !(t.kind == Kind::Word && RESERVED.contains(&t.keyword(sql).as_str()))) {
                label = alias.name(sql);
                index += 1;
            }
            if !label.is_empty() {
                self.entries.push(ScopeEntry { label, names, columns });
            }
            if list && tokens.get(index).is_some_and(|t| t.is_punct(',')) {
                index += 1;
            } else {
                return index;
            }
        }
    }
}

/// The output column names of a query: `AS` names, plain column names, and
/// the last part of `table.column`. Expressions without a name are left out.
fn select_list(tokens: &[&Token], sql: &str) -> Vec<String> {
    let Some(select) = top_level(tokens).find(|(_, t)| t.keyword(sql) == "select").map(|(index, _)| index) else {
        return Vec::new();
    };
    let end = top_level(tokens)
        .find(|(index, t)| {
            *index > select
                && matches!(t.keyword(sql).as_str(), "from" | "into" | "where" | "group" | "order" | "limit" | "union" | "having" | "window")
        })
        .map_or(tokens.len(), |(index, _)| index);
    let mut columns = Vec::new();
    let mut item: Vec<&Token> = Vec::new();
    let mut depth = 0;
    for token in tokens[select + 1..end].iter().copied().chain(std::iter::once(&COMMA)) {
        if token.is_punct('(') {
            depth += 1;
        } else if token.is_punct(')') {
            depth -= 1;
        }
        if depth == 0 && token.is_punct(',') {
            if let Some(name) = output_name(&item, sql) {
                columns.push(name);
            }
            item.clear();
        } else {
            item.push(token);
        }
    }
    columns
}

static COMMA: Token = Token { kind: Kind::Punct(','), start: 0, end: 0, opaque: false, closed: true };

fn output_name(item: &[&Token], sql: &str) -> Option<String> {
    let item: Vec<&&Token> = item.iter().filter(|t| !matches!(t.keyword(sql).as_str(), "distinct" | "all")).collect();
    let last = item.last()?;
    if !last.is_name() {
        return None;
    }
    match item.len() {
        1 => Some(last.name(sql)),
        _ => {
            let previous = item[item.len() - 2];
            // `expr AS name`, `table.name`, or `expr name`.
            let named = previous.keyword(sql) == "as" || previous.is_punct('.') || previous.is_name() || previous.is_punct(')');
            named.then(|| last.name(sql))
        }
    }
}

/// Tokens at bracket depth 0, with their index.
fn top_level<'a>(tokens: &'a [&'a Token]) -> impl Iterator<Item = (usize, &'a Token)> + 'a {
    let mut depth = 0i32;
    tokens.iter().enumerate().filter_map(move |(index, token)| {
        let top = depth == 0;
        if token.is_punct('(') {
            depth += 1;
        } else if token.is_punct(')') {
            depth -= 1;
        }
        (top && !token.is_punct('(')).then_some((index, *token))
    })
}

/// The index of the `)` that closes the `(` at `open`, or the last token.
fn matching(tokens: &[&Token], open: usize) -> usize {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.is_punct('(') {
            depth += 1;
        } else if token.is_punct(')') {
            depth -= 1;
            if depth == 0 {
                return index;
            }
        }
    }
    tokens.len().saturating_sub(1).max(open)
}

// MARK: - Tokens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    QuotedName,
    Text,
    Number,
    Punct(char),
}

#[derive(Debug)]
struct Token {
    kind: Kind,
    /// Byte range in the SQL.
    start: usize,
    end: usize,
    /// A string or a comment: its content is not SQL.
    opaque: bool,
    /// False for a string, quoted name or comment that runs to the end.
    closed: bool,
}

impl Token {
    fn is_punct(&self, c: char) -> bool {
        self.kind == Kind::Punct(c)
    }

    fn is_name(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::QuotedName)
    }

    /// The name it stands for: a plain word as written, a quoted name without
    /// its quotes.
    fn name(&self, sql: &str) -> String {
        let text = &sql[self.start..self.end];
        match self.kind {
            Kind::QuotedName => {
                // "name", or MySQL's `name`.
                let quote = text.chars().next().unwrap_or('"');
                let inner = text.strip_prefix(quote).unwrap_or(text);
                // An unclosed name (still being typed) has no closing quote.
                let inner = if self.closed { inner.strip_suffix(quote).unwrap_or(inner) } else { inner };
                inner.replace(&format!("{quote}{quote}"), &quote.to_string())
            }
            _ => text.to_string(),
        }
    }

    /// The word in lowercase, for comparing with keywords; empty otherwise.
    fn keyword(&self, sql: &str) -> String {
        if self.kind == Kind::Word { sql[self.start..self.end].to_ascii_lowercase() } else { String::new() }
    }
}

fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        let rest = &sql[index..];
        let (kind, end, opaque, closed) = if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        } else if rest.starts_with("--") {
            let end = rest.find('\n').map_or(bytes.len(), |offset| index + offset);
            (Kind::Text, end, true, end < bytes.len())
        } else if rest.starts_with("/*") {
            match rest[2..].find("*/") {
                Some(offset) => (Kind::Text, index + 2 + offset + 2, true, true),
                None => (Kind::Text, bytes.len(), true, false),
            }
        } else if byte == b'\'' || byte == b'"' || byte == b'`' {
            let (end, closed) = quoted_end(bytes, index, byte);
            (if byte == b'\'' { Kind::Text } else { Kind::QuotedName }, end, byte == b'\'', closed)
        } else if byte == b'$' {
            match dollar_tag(rest) {
                Some(tag) => match rest[tag.len()..].find(tag) {
                    Some(offset) => (Kind::Text, index + tag.len() + offset + tag.len(), true, true),
                    None => (Kind::Text, bytes.len(), true, false),
                },
                None => (Kind::Punct('$'), index + 1, false, true),
            }
        } else if byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80 {
            let end = rest
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
                .map_or(bytes.len(), |(offset, _)| index + offset);
            (Kind::Word, end, false, true)
        } else if byte.is_ascii_digit() {
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(bytes.len(), |offset| index + offset);
            (Kind::Number, end, false, true)
        } else {
            (Kind::Punct(byte as char), index + 1, false, true)
        };
        tokens.push(Token { kind, start, end, opaque, closed });
        index = end;
    }
    tokens
}

/// The end of a quoted string or name that starts at `start`; a doubled
/// quote is part of it.
fn quoted_end(bytes: &[u8], start: usize, quote: u8) -> (usize, bool) {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return (index + 1, true);
        }
        index += 1;
    }
    (bytes.len(), false)
}

/// `$$` or `$tag$` at the start of `text`.
fn dollar_tag(text: &str) -> Option<&str> {
    let close = text[1..].find(|c: char| !(c.is_alphanumeric() || c == '_'))? + 1;
    (text.as_bytes().get(close) == Some(&b'$')).then(|| &text[..=close])
}

impl Completer {
    /// The name as SQL: bare when it is a plain identifier, else quoted.
    /// PostgreSQL folds bare names to lowercase, so there only lowercase
    /// names stay bare; MySQL keeps their case.
    fn quote(&self, name: &str) -> String {
        let lowercase_only = !self.backticks;
        let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_' || (!lowercase_only && c.is_ascii_uppercase()))
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$' || (!lowercase_only && c.is_ascii_uppercase()))
            && !RESERVED.contains(&name.to_ascii_lowercase().as_str());
        match (plain, self.backticks) {
            (true, _) => name.to_string(),
            (false, true) => crate::mysql::backtick(name),
            (false, false) => crate::rows::quote_identifier(name),
        }
    }
}

fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (offset, c) in text.char_indices() {
        if units >= utf16 {
            return offset;
        }
        units += c.len_utf16();
    }
    text.len()
}

fn utf16_offset(text: &str, byte: usize) -> u32 {
    text[..byte.min(text.len())].encode_utf16().count() as u32
}

/// Words that can't be an alias (they start the next clause).
const RESERVED: &[&str] = &[
    "all", "and", "as", "asc", "by", "case", "cross", "desc", "distinct", "else", "end", "except", "fetch", "for", "from",
    "full", "group", "having", "in", "inner", "intersect", "into", "is", "join", "lateral", "left", "limit", "natural",
    "not", "null", "offset", "on", "or", "order", "outer", "returning", "right", "select", "set", "table", "then", "union",
    "using", "values", "when", "where", "window", "with",
];

const STATEMENT_KEYWORDS: &[&str] = &[
    "select", "insert", "update", "delete", "with", "create", "alter", "drop", "truncate", "explain", "begin", "commit",
    "rollback", "grant", "revoke", "show", "set", "copy", "vacuum", "analyze", "values", "table", "call", "do",
];

const KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "in", "is", "null", "as", "on", "join", "left", "right", "inner",
    "outer", "full", "cross", "lateral", "natural", "using", "group", "by", "order", "having", "limit", "offset",
    "fetch", "insert", "into", "values", "update", "set", "delete", "returning", "create", "table", "index", "view",
    "materialized", "drop", "alter", "add", "column", "constraint", "primary", "key", "foreign", "references",
    "unique", "check", "default", "with", "recursive", "union", "intersect", "except", "all", "distinct", "case",
    "when", "then", "else", "end", "begin", "commit", "rollback", "asc", "desc", "nulls", "first", "last", "like",
    "ilike", "similar", "between", "exists", "any", "some", "true", "false", "explain", "analyze", "truncate",
    "cascade", "restrict", "grant", "revoke", "cast", "interval", "current_date", "current_time",
    "current_timestamp", "current_user", "over", "partition", "window", "filter", "conflict", "do", "nothing",
    "returning", "schema", "sequence", "function", "trigger", "extension", "if", "only", "for", "share",
    // Parsed as syntax, not functions, so pg_proc doesn't list them.
    "coalesce", "nullif", "greatest", "least",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn completer() -> Completer {
        let relation = |schema: &str, name: &str, kind, columns: &[&str]| CatalogRelation {
            schema: schema.into(),
            name: name.into(),
            kind,
            columns: columns.iter().map(|c| c.to_string()).collect(),
        };
        Completer {
            relations: vec![
                relation("public", "customers", RelationKind::Table, &["id", "name", "country", "plan"]),
                relation("public", "orders", RelationKind::Table, &["id", "customer_id", "status", "total", "created_at"]),
                relation("public", "revenue_by_country", RelationKind::View, &["country", "revenue"]),
                relation("billing", "Invoices", RelationKind::Table, &["id", "Amount"]),
                relation("pg_catalog", "pg_stat_activity", RelationKind::View, &["pid", "query"]),
            ],
            functions: vec![
                ("pg_catalog".into(), "count".into()),
                ("pg_catalog".into(), "current_setting".into()),
                ("public".into(), "customer_score".into()),
            ],
            schemas: vec!["billing".into(), "public".into()],
            search_path: vec!["pg_catalog".into(), "public".into()],
            backticks: false,
        }
    }

    /// Completions with the cursor at `|`.
    fn at(sql: &str) -> Completions {
        let cursor = sql.find('|').unwrap();
        let text = sql.replace('|', "");
        completer().complete(text.clone(), utf16_offset(&text, cursor), false, KeywordCase::AsTyped)
    }

    fn labels(completions: &Completions) -> Vec<String> {
        completions.items.iter().map(|item| format!("{} {:?}", item.label, item.kind)).collect()
    }

    #[test]
    fn tables_after_from() {
        let result = at("select * from cus|");
        assert_eq!(result.replace_start, 14);
        assert_eq!(result.items[0].label, "customers");
        assert_eq!(result.items[0].kind, CompletionKind::Table);
        assert_eq!(result.items[0].detail, "public");
    }

    #[test]
    fn tables_outside_the_search_path_are_qualified() {
        let result = at("select * from inv|");
        assert_eq!(result.items[0].insert_text, r#"billing."Invoices""#);
    }

    #[test]
    fn letters_in_order_match_too() {
        // `sl` for select, as a fast typist types it.
        assert_eq!(at("sl|").items[0].label, "select");
        // Prefix matches come first: `cou` is country before customer_id…
        let result = at("select * from orders o join customers c on c.id = o.customer_id where cou|");
        assert_eq!(result.items[0].label, "country");
        // …and the first letter must agree: `ountry` is no match.
        assert!(at("select * from customers where ountry|").items.iter().all(|item| item.label != "country"));
        assert_eq!(match_score("count", "cnt"), Some(5));
        assert_eq!(match_score("country", "cnt"), Some(5));
        assert_eq!(match_score("count", "xc"), None);
    }

    #[test]
    fn quick_open_ranking() {
        let names: Vec<String> = ["orders", "order_items", "customers", "customer_score", "revenue_by_country", "products"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let ranked = |query: &str| -> Vec<&str> {
            rank_names(query.into(), names.clone()).into_iter().map(|index| names[index as usize].as_str()).collect()
        };
        // Prefix first, shorter first.
        assert_eq!(ranked("ord"), ["orders", "order_items"]);
        // A word start beats a substring elsewhere.
        assert_eq!(ranked("it")[0], "order_items");
        // Letters in order: `rbc` finds revenue_by_country.
        assert_eq!(ranked("rbc"), ["revenue_by_country"]);
        assert_eq!(ranked("CUS"), ["customers", "customer_score"]);
        assert!(ranked("zzz").is_empty());
        assert_eq!(ranked("").len(), names.len());
    }

    #[test]
    fn keyword_case_setting() {
        let complete = |sql: &str, case| completer().complete(sql.into(), sql.len() as u32, false, case).items[0].insert_text.clone();
        assert_eq!(complete("sel", KeywordCase::Upper), "SELECT");
        assert_eq!(complete("SEL", KeywordCase::Lower), "select");
        // Names keep their case whatever the setting.
        assert_eq!(complete("select * from cus", KeywordCase::Upper), "customers");
    }

    #[test]
    fn keywords_at_the_start_follow_the_typed_case() {
        assert_eq!(at("sel|").items[0].insert_text, "select");
        assert_eq!(at("SEL|").items[0].insert_text, "SELECT");
        assert_eq!(at("select 1; up|").items[0].label, "update");
    }

    #[test]
    fn columns_of_an_alias() {
        let result = at("select o.| from orders o join customers c on c.id = o.customer_id");
        assert_eq!(labels(&result), ["id Column", "customer_id Column", "status Column", "total Column", "created_at Column"]);
        assert_eq!(result.items[0].detail, "o");
        let result = at("select o.id from orders o join customers c on c.| ");
        assert_eq!(result.items.len(), 4);
    }

    #[test]
    fn columns_in_scope_in_where() {
        let result = at("select * from customers where cou|");
        assert_eq!(result.items[0].label, "country");
        assert_eq!(result.items[0].detail, "customers");
        // Functions too: `count` matches the same prefix.
        assert!(labels(&result).contains(&"count Function".to_string()));
    }

    #[test]
    fn tables_in_scope_after_their_columns() {
        let result = at("select * from customers where cu|");
        assert_eq!(result.items[0].label, "customers");
        assert_eq!(result.items[0].kind, CompletionKind::Table);
        // An alias shows the table it stands for.
        let result = at("select * from orders o where o|");
        let alias = result.items.iter().find(|item| item.label == "o").expect("the alias is offered");
        assert_eq!(alias.detail, "orders");
    }

    #[test]
    fn tables_before_keywords_without_from() {
        let result = at("select * c|");
        assert_eq!(result.items[0].label, "customers");
    }

    #[test]
    fn columns_of_a_cte_and_a_subquery() {
        let sql = "with recent_orders as (select id, customer_id as buyer, total, now() - created_at age from orders)
                   select ro.| from recent_orders ro";
        assert_eq!(labels(&at(sql)), ["id Column", "buyer Column", "total Column", "age Column"]);
        let sql = "select x.| from (select name, count(*) as n from customers group by name) x";
        assert_eq!(labels(&at(sql)), ["name Column", "n Column"]);
        // The CTE is offered as a table after FROM.
        let sql = "with recent as (select 1 as one) select * from rec|";
        assert_eq!(at(sql).items[0].detail, "CTE");
    }

    #[test]
    fn nothing_inside_strings_and_comments() {
        assert!(at("select * from customers where name = 'cus|'").items.is_empty());
        assert!(at("select 1 -- from cus|").items.is_empty());
        assert!(at("select $$ from cus| $$").items.is_empty());
    }

    #[test]
    fn nothing_for_an_empty_word_unless_asked() {
        assert!(at("select * from |").items.is_empty());
        let text = "select * from ";
        let explicit = completer().complete(text.into(), utf16_offset(text, text.len()), true, KeywordCase::AsTyped);
        assert_eq!(explicit.items[0].label, "customers");
    }

    #[test]
    fn schema_qualifier_lists_its_relations() {
        let result = at("select * from billing.|");
        assert_eq!(labels(&result), ["Invoices Table"]);
        assert_eq!(result.items[0].insert_text, r#""Invoices""#);
    }

    #[test]
    fn system_catalogs_only_for_their_prefix() {
        assert!(at("select * from p|").items.iter().all(|item| item.label != "pg_stat_activity"));
        assert_eq!(at("select * from pg_st|").items[0].label, "pg_stat_activity");
    }

    #[test]
    fn quoted_names_with_doubled_quotes() {
        let sql = r#"select i.| from billing."Invoices" i"#;
        assert_eq!(labels(&at(sql)), ["id Column", "Amount Column"]);
        assert_eq!(at(sql).items[1].insert_text, r#""Amount""#);
    }

    #[test]
    fn utf16_offsets_after_non_ascii_text() {
        let result = at("select 'café', * from cus|");
        assert_eq!(result.replace_start, 22);
        assert_eq!(result.items[0].label, "customers");
    }
}

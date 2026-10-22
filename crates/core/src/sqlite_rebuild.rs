//! Table changes SQLite can't make with ALTER TABLE: a column's type, NULL
//! or default, and adding or dropping a constraint. They follow SQLite's own
//! recipe (https://sqlite.org/lang_altertable.html#otheralter): create the
//! changed table under a new name, copy the rows, drop the old table, rename
//! the new one, and recreate the indexes and triggers.
//!
//! The new definition is the table's own CREATE TABLE text with only the
//! changed part rewritten, so CHECK, COLLATE, AUTOINCREMENT and the rest
//! survive. Rebuilding it from the catalog would lose them.

use crate::rows::{quote_identifier, quote_literal};
use crate::schema::Relation;
use crate::session::DatabaseSession;
use crate::structure::StructureChange;
use crate::DatabaseError;

/// What [`DatabaseSession::sqlite_rebuild`] runs: the statements, in one
/// transaction with foreign keys off, then a foreign key check of `checked`.
pub(crate) struct Rebuild {
    pub(crate) statements: Vec<String>,
    pub(crate) checked: Vec<String>,
}

/// True when SQLite can only make `change` by rebuilding the table.
pub(crate) fn needs_rebuild(change: &StructureChange) -> bool {
    match change {
        StructureChange::EditColumn { original, column } => {
            column.column_type.trim() != original.column_type
                || column.nullable != original.nullable
                || non_empty(&column.default_value) != non_empty(&original.default_value)
        }
        StructureChange::AddForeignKey { .. } | StructureChange::DropConstraint { .. } => true,
        _ => false,
    }
}

impl DatabaseSession {
    pub(crate) async fn sqlite_rebuild(&self, relation: &Relation, change: StructureChange) -> Result<Rebuild, DatabaseError> {
        let schema = quote_identifier(&relation.schema);
        let table_literal = quote_literal(&relation.name)?;
        let schema_literal = quote_literal(&relation.schema)?;
        let first = |rows: Vec<Vec<Option<String>>>| rows.into_iter().next().and_then(|row| row.into_iter().next()).flatten();

        let sql = first(
            self.text_rows(format!("SELECT sql FROM {schema}.sqlite_master WHERE type = 'table' AND name = {table_literal}"))
                .await?,
        )
        .ok_or_else(|| DatabaseError::new(format!("The table {} was not found.", relation.name)))?;
        let mut table = CreateTable::parse(&sql)?;

        let mut rename = None;
        match change {
            StructureChange::EditColumn { original, column } => {
                if non_empty(&column.comment).is_some() {
                    return Err(DatabaseError::new("SQLite has no column comments."));
                }
                let new_name = column.name.trim();
                if new_name.is_empty() {
                    return Err(DatabaseError::new("The column needs a name."));
                }
                if new_name != original.name {
                    rename = Some((original.name.clone(), new_name.to_string()));
                }
                let index = table.column(&original.name)?;
                let mut definition = ColumnDefinition::parse(&table.items[index]);
                let column_type = column.column_type.trim();
                if column_type != original.column_type {
                    definition.column_type = column_type.to_string();
                }
                if column.nullable != original.nullable {
                    definition.remove(|segment| matches!(segment, Segment::NotNull | Segment::Null));
                    if !column.nullable {
                        definition.added.push("NOT NULL".into());
                    }
                }
                if non_empty(&column.default_value) != non_empty(&original.default_value) {
                    definition.remove(|segment| matches!(segment, Segment::Default));
                    if let Some(default) = non_empty(&column.default_value) {
                        definition.added.push(format!("DEFAULT {}", default_expression(default)));
                    }
                }
                table.items[index] = definition.text();
            }
            StructureChange::AddForeignKey { name, column, referenced, referenced_column, on_delete } => {
                // SQLite resolves a foreign key in the table's own schema.
                if referenced.schema != relation.schema {
                    return Err(DatabaseError::new("In SQLite, a foreign key must point at a table in the same schema."));
                }
                table.column(&column)?;
                let mut item = String::new();
                if let Some(name) = name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                    item += &format!("CONSTRAINT {} ", quote_identifier(name));
                }
                item += &format!(
                    "FOREIGN KEY ({}) REFERENCES {} ({})",
                    quote_identifier(&column),
                    quote_identifier(&referenced.name),
                    quote_identifier(&referenced_column)
                );
                if let Some(action) = on_delete.as_deref().map(str::trim).filter(|action| !action.is_empty()) {
                    let action = action.to_uppercase();
                    if !crate::structure::REFERENTIAL_ACTIONS.contains(&action.as_str()) {
                        return Err(DatabaseError::new(format!("Unknown ON DELETE action: {action}")));
                    }
                    item += &format!(" ON DELETE {action}");
                }
                table.items.push(item);
            }
            StructureChange::DropConstraint { name, .. } => {
                if name == "primary key" {
                    return Err(DatabaseError::new("TablePort can't drop a SQLite primary key: it may be the table's row number."));
                }
                let target = if let Some(number) = name.strip_prefix("foreign key ").and_then(|number| number.parse::<usize>().ok()) {
                    // The Structure view numbers foreign keys in SQLite's order.
                    let rows = self
                        .text_rows(format!(
                            "SELECT id, \"from\", \"table\" FROM pragma_foreign_key_list({table_literal}, {schema_literal}) ORDER BY id, seq"
                        ))
                        .await?;
                    let mut keys: Vec<(String, Vec<String>, String)> = Vec::new();
                    for row in rows {
                        let [Some(id), Some(from), Some(referenced)] = <[Option<String>; 3]>::try_from(row).unwrap_or_default() else {
                            continue;
                        };
                        match keys.last_mut().filter(|key| key.0 == id) {
                            Some(key) => key.1.push(from),
                            None => keys.push((id, vec![from], referenced)),
                        }
                    }
                    let (_, columns, referenced) = number
                        .checked_sub(1)
                        .and_then(|index| keys.into_iter().nth(index))
                        .ok_or_else(|| DatabaseError::new(format!("No constraint named {name}.")))?;
                    Target::ForeignKey { columns, referenced }
                } else if !name.starts_with("sqlite_autoindex_") {
                    Target::Named(name)
                } else {
                    let columns = self
                        .text_rows(format!(
                            "SELECT l.origin, i.name FROM pragma_index_list({table_literal}, {schema_literal}) l, \
                             pragma_index_info(l.name, {schema_literal}) i WHERE l.name = {} ORDER BY i.seqno",
                            quote_literal(&name)?
                        ))
                        .await?;
                    if columns.first().and_then(|row| row.first().cloned().flatten()).as_deref() != Some("u") {
                        return Err(DatabaseError::new(format!("No constraint named {name}.")));
                    }
                    Target::Unique { columns: columns.into_iter().filter_map(|row| row.get(1).cloned().flatten()).collect() }
                };
                table.remove(&target)?;
            }
            _ => return Err(DatabaseError::new("SQLite makes this change in place.")),
        }

        let new_name = format!("_tableport_new_{}", relation.name);
        let quoted_table = format!("{schema}.{}", quote_identifier(&relation.name));
        let quoted_new = format!("{schema}.{}", quote_identifier(&new_name));
        // Generated columns can't be inserted into; they compute themselves.
        let columns = self
            .text_rows(format!("SELECT name FROM pragma_table_xinfo({table_literal}, {schema_literal}) WHERE hidden = 0 ORDER BY cid"))
            .await?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .map(|name| quote_identifier(&name))
            .collect::<Vec<_>>()
            .join(", ");
        // Views, and triggers of other tables, may name the table; while it is
        // gone, SQLite refuses the rename over them. They step aside and come
        // back as they were. (libSQL has no legacy_alter_table to avoid this.)
        let bystanders: Vec<(String, String, String)> = self
            .text_rows(format!(
                "SELECT type, name, sql FROM {schema}.sqlite_master WHERE sql IS NOT NULL \
                 AND (type = 'view' OR (type = 'trigger' AND tbl_name <> {table_literal})) ORDER BY rowid"
            ))
            .await?
            .into_iter()
            .filter_map(|row| {
                let [Some(kind), Some(name), Some(sql)] = <[Option<String>; 3]>::try_from(row).ok()? else { return None };
                Some((kind, name, sql))
            })
            .collect();
        let mut statements = vec![
            table.sql(&quoted_new),
            format!("INSERT INTO {quoted_new} ({columns}) SELECT {columns} FROM {quoted_table};"),
        ];
        for (kind, name, _) in &bystanders {
            statements.push(format!("DROP {} {schema}.{};", kind.to_uppercase(), quote_identifier(name)));
        }
        statements.extend([
            format!("DROP TABLE {quoted_table};"),
            format!("ALTER TABLE {quoted_new} RENAME TO {};", quote_identifier(&relation.name)),
        ]);
        // Copying the rows restarts AUTOINCREMENT at the highest id; ids
        // of rows deleted earlier must stay used.
        if table.autoincrement {
            if let Some(sequence) =
                first(self.text_rows(format!("SELECT seq FROM {schema}.sqlite_sequence WHERE name = {table_literal}")).await?)
            {
                let sequence: i64 = sequence.parse().map_err(|_| DatabaseError::new("The AUTOINCREMENT counter is not a number."))?;
                statements.push(format!("UPDATE {schema}.sqlite_sequence SET seq = max(seq, {sequence}) WHERE name = {table_literal};"));
            }
        }
        let dependents = self
            .text_rows(format!(
                "SELECT sql FROM {schema}.sqlite_master WHERE tbl_name = {table_literal} \
                 AND type IN ('index', 'trigger') AND sql IS NOT NULL ORDER BY type = 'trigger', rowid"
            ))
            .await?;
        for sql in dependents.into_iter().filter_map(|row| row.into_iter().next().flatten()) {
            statements.push(qualify_create(&sql, &relation.schema));
        }
        for (_, _, sql) in &bystanders {
            statements.push(qualify_create(sql, &relation.schema));
        }
        if let Some((old, new)) = rename {
            statements.push(format!("ALTER TABLE {quoted_table} RENAME COLUMN {} TO {};", quote_identifier(&old), quote_identifier(&new)));
        }

        // The table's own keys, and the tables whose keys point at it.
        let mut checked = vec![relation.name.clone()];
        let children = self
            .text_rows(format!(
                "SELECT DISTINCT m.name FROM {schema}.sqlite_master m, pragma_foreign_key_list(m.name, {schema_literal}) f \
                 WHERE m.type = 'table' AND f.\"table\" = {table_literal} COLLATE NOCASE"
            ))
            .await?;
        checked.extend(children.into_iter().filter_map(|row| row.into_iter().next().flatten()).filter(|name| *name != relation.name));
        Ok(Rebuild { statements, checked })
    }
}

fn non_empty(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|text| !text.is_empty())
}

/// SQLite takes a literal as a default, and any other expression only in
/// parentheses.
pub(crate) fn default_expression(default: &str) -> String {
    let tokens = tokenize(default);
    let literal = match tokens.as_slice() {
        [token] => {
            matches!(token.kind, Kind::Text | Kind::Number)
                || (token.kind == Kind::Word
                    && ["NULL", "TRUE", "FALSE", "CURRENT_TIME", "CURRENT_DATE", "CURRENT_TIMESTAMP"]
                        .contains(&default.trim().to_uppercase().as_str()))
        }
        [sign, number] => matches!(&default[sign.start..sign.end], "-" | "+") && number.kind == Kind::Number,
        [open, .., close] => &default[open.start..open.end] == "(" && matching(&tokens, default, 0) == Some(tokens.len() - 1) && &default[close.start..close.end] == ")",
        _ => false,
    };
    if literal {
        default.to_string()
    } else {
        format!("({default})")
    }
}

/// Puts the schema on an index's, trigger's or view's name, so it is
/// recreated in the table's schema: the saved SQL names it without one.
fn qualify_create(sql: &str, schema: &str) -> String {
    let tokens = tokenize(sql);
    let words: Vec<String> = tokens.iter().map(|token| sql[token.start..token.end].to_uppercase()).collect();
    let Some(mut position) = words.iter().position(|word| word == "INDEX" || word == "TRIGGER" || word == "VIEW") else {
        return sql.to_string();
    };
    position += 1;
    if words.get(position).map(String::as_str) == Some("IF") {
        position += 3;
    }
    let Some(name) = tokens.get(position) else { return sql.to_string() };
    if tokens.get(position + 1).is_some_and(|next| &sql[next.start..next.end] == ".") {
        return sql.to_string();
    }
    format!("{}{}.{}", &sql[..name.start], quote_identifier(schema), &sql[name.start..])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    /// A quoted name: "a", [a] or `a`.
    Name,
    /// A string: 'a'.
    Text,
    Number,
    Symbol,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// SQLite's tokens, without spaces and comments. Offsets are byte offsets.
fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        let kind = match byte {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {
                index += 1;
                continue;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index < bytes.len() && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/')) {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                continue;
            }
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if byte == b'[' { b']' } else { byte };
                index += 1;
                loop {
                    match bytes.get(index) {
                        None => break,
                        // A doubled quote is a quote inside; brackets don't double.
                        Some(&next) if next == close && close != b']' && bytes.get(index + 1) == Some(&close) => index += 2,
                        Some(&next) if next == close => {
                            index += 1;
                            break;
                        }
                        Some(_) => index += 1,
                    }
                }
                if byte == b'\'' {
                    Kind::Text
                } else {
                    Kind::Name
                }
            }
            b'0'..=b'9' | b'.' if byte != b'.' || bytes.get(index + 1).is_some_and(u8::is_ascii_digit) => {
                while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'.') {
                    index += 1;
                }
                Kind::Number
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$') || bytes[index] >= 0x80) {
                    index += 1;
                }
                Kind::Word
            }
            _ => {
                // One character, which may be several bytes.
                index += sql[index..].chars().next().map_or(1, char::len_utf8);
                Kind::Symbol
            }
        };
        tokens.push(Token { kind, start, end: index });
    }
    tokens
}

/// The index of the ")" that closes the "(" at `open`.
fn matching(tokens: &[Token], sql: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match &sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// A name as SQLite reads it: without its quotes.
fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    match (chars.next(), text.chars().last()) {
        (Some('"'), Some('"')) if text.len() >= 2 => text[1..text.len() - 1].replace("\"\"", "\""),
        (Some('`'), Some('`')) if text.len() >= 2 => text[1..text.len() - 1].replace("``", "`"),
        (Some('['), Some(']')) if text.len() >= 2 => text[1..text.len() - 1].to_string(),
        _ => text.to_string(),
    }
}

/// The constraint to take out of a table.
enum Target {
    ForeignKey { columns: Vec<String>, referenced: String },
    Unique { columns: Vec<String> },
    /// One declared with CONSTRAINT name.
    Named(String),
}

/// The names given with CONSTRAINT to a table's foreign keys, by their
/// columns and referenced table; SQLite's catalog doesn't keep them.
pub(crate) fn foreign_key_names(sql: &str) -> Vec<(Vec<String>, String, String)> {
    let Ok(table) = CreateTable::parse(sql) else { return Vec::new() };
    let mut names = Vec::new();
    for item in &table.items {
        let tokens = tokenize(item);
        let words: Vec<String> = tokens.iter().map(|token| item[token.start..token.end].to_uppercase()).collect();
        let text = |index: usize| tokens.get(index).map(|token| unquote(&item[token.start..token.end])).unwrap_or_default();
        if !CreateTable::is_column(item) {
            if words.first().map(String::as_str) != Some("CONSTRAINT") || words.get(2).map(String::as_str) != Some("FOREIGN") {
                continue;
            }
            let Some(close) = matching(&tokens, item, 4) else { continue };
            let columns = tokens[5..close].iter().filter(|token| token.kind != Kind::Symbol).map(|token| unquote(&item[token.start..token.end])).collect();
            names.push((columns, text(close + 2), text(1)));
            continue;
        }
        for (segment, clause) in ColumnDefinition::parse(item).segments {
            if let (Segment::References(referenced), true) = (segment, clause.to_uppercase().starts_with("CONSTRAINT")) {
                let clause_tokens = tokenize(&clause);
                let name = clause_tokens.get(1).map(|token| unquote(&clause[token.start..token.end])).unwrap_or_default();
                names.push((vec![text(0)], referenced, name));
            }
        }
    }
    names
}

/// A CREATE TABLE statement split into its column and constraint items.
struct CreateTable {
    items: Vec<String>,
    /// After the closing parenthesis: WITHOUT ROWID, STRICT.
    options: String,
    autoincrement: bool,
}

const TABLE_CONSTRAINTS: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

impl CreateTable {
    fn parse(sql: &str) -> Result<Self, DatabaseError> {
        let unreadable = || DatabaseError::new("TablePort could not read this table's CREATE TABLE statement.");
        let tokens = tokenize(sql);
        let open = tokens.iter().position(|token| &sql[token.start..token.end] == "(").ok_or_else(unreadable)?;
        let close = matching(&tokens, sql, open).ok_or_else(unreadable)?;
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut item_start = tokens[open].end;
        for token in &tokens[open + 1..close] {
            match &sql[token.start..token.end] {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                "," if depth == 0 => {
                    items.push(sql[item_start..token.start].trim().to_string());
                    item_start = token.end;
                }
                _ => {}
            }
        }
        items.push(sql[item_start..tokens[close].start].trim().to_string());
        let options = sql[tokens[close].end..].trim().trim_end_matches(';').trim().to_string();
        let autoincrement = tokens.iter().any(|token| sql[token.start..token.end].eq_ignore_ascii_case("AUTOINCREMENT"));
        Ok(Self { items, options, autoincrement })
    }

    fn is_column(item: &str) -> bool {
        tokenize(item)
            .first()
            .is_some_and(|token| token.kind != Kind::Word || !TABLE_CONSTRAINTS.contains(&item[token.start..token.end].to_uppercase().as_str()))
    }

    fn column(&self, name: &str) -> Result<usize, DatabaseError> {
        self.items
            .iter()
            .position(|item| {
                Self::is_column(item) && tokenize(item).first().is_some_and(|token| unquote(&item[token.start..token.end]).eq_ignore_ascii_case(name))
            })
            .ok_or_else(|| DatabaseError::new(format!("The column {name} was not found in the table's definition.")))
    }

    /// Takes the constraint out: a table item, or a clause of a column.
    fn remove(&mut self, target: &Target) -> Result<(), DatabaseError> {
        let same = |names: &[String], wanted: &[String]| {
            names.len() == wanted.len() && names.iter().zip(wanted).all(|(name, wanted)| name.eq_ignore_ascii_case(wanted))
        };
        for index in 0..self.items.len() {
            let item = self.items[index].clone();
            let tokens = tokenize(&item);
            let words: Vec<String> = tokens.iter().map(|token| item[token.start..token.end].to_uppercase()).collect();
            let named = |offset: usize| {
                matches!(target, Target::Named(name) if words.get(offset).map(String::as_str) == Some("CONSTRAINT")
                    && tokens.get(offset + 1).is_some_and(|token| unquote(&item[token.start..token.end]).eq_ignore_ascii_case(name)))
            };
            if !Self::is_column(&item) {
                if named(0) {
                    self.items.remove(index);
                    return Ok(());
                }
                // [CONSTRAINT name] FOREIGN KEY (columns) REFERENCES table … / UNIQUE (columns) …
                let start = if words.first().map(String::as_str) == Some("CONSTRAINT") { 2 } else { 0 };
                let (keyword, open) = match words.get(start).map(String::as_str) {
                    Some("FOREIGN") => ("FOREIGN", start + 2),
                    Some("UNIQUE") => ("UNIQUE", start + 1),
                    _ => continue,
                };
                let Some(close) = (words.get(open).map(String::as_str) == Some("(")).then(|| matching(&tokens, &item, open)).flatten() else {
                    continue;
                };
                let names: Vec<String> =
                    tokens[open + 1..close].iter().filter(|token| token.kind != Kind::Symbol).map(|token| unquote(&item[token.start..token.end])).collect();
                let found = match target {
                    Target::ForeignKey { columns, referenced } => {
                        keyword == "FOREIGN"
                            && same(&names, columns)
                            && tokens.get(close + 2).is_some_and(|token| unquote(&item[token.start..token.end]).eq_ignore_ascii_case(referenced))
                    }
                    Target::Unique { columns } => keyword == "UNIQUE" && same(&names, columns),
                    Target::Named(_) => false,
                };
                if found {
                    self.items.remove(index);
                    return Ok(());
                }
                continue;
            }
            let column = tokens.first().map(|token| unquote(&item[token.start..token.end])).unwrap_or_default();
            let mut definition = ColumnDefinition::parse(&item);
            let before = definition.segments.len();
            match target {
                Target::ForeignKey { columns, referenced } if same(std::slice::from_ref(&column), columns) => definition.remove(|segment| match segment {
                    Segment::References(table) => table.eq_ignore_ascii_case(referenced),
                    _ => false,
                }),
                Target::Unique { columns } if same(std::slice::from_ref(&column), columns) => definition.remove(|segment| matches!(segment, Segment::Unique)),
                Target::Named(name) => {
                    let named = |clause: &str| {
                        let tokens = tokenize(clause);
                        tokens.first().is_some_and(|token| clause[token.start..token.end].eq_ignore_ascii_case("CONSTRAINT"))
                            && tokens.get(1).is_some_and(|token| unquote(&clause[token.start..token.end]).eq_ignore_ascii_case(name))
                    };
                    definition.segments.retain(|(_, clause)| !named(clause));
                }
                _ => continue,
            }
            if definition.segments.len() < before {
                self.items[index] = definition.text();
                return Ok(());
            }
        }
        Err(DatabaseError::new("TablePort could not find this constraint in the table's CREATE TABLE statement."))
    }

    fn sql(&self, name: &str) -> String {
        let options = if self.options.is_empty() { String::new() } else { format!(" {}", self.options) };
        format!("CREATE TABLE {name} (\n  {}\n){options};", self.items.join(",\n  "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    NotNull,
    Null,
    Default,
    Unique,
    /// REFERENCES and the table's name.
    References(String),
    Other,
}

/// One column's definition: its name, type and constraint clauses, each
/// kept as written.
struct ColumnDefinition {
    name: String,
    column_type: String,
    segments: Vec<(Segment, String)>,
    added: Vec<String>,
}

const COLUMN_CONSTRAINTS: &[&str] =
    &["CONSTRAINT", "PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES", "GENERATED", "AS"];

impl ColumnDefinition {
    fn parse(item: &str) -> Self {
        let tokens = tokenize(item);
        let words: Vec<String> = tokens.iter().map(|token| item[token.start..token.end].to_uppercase()).collect();
        let starts_clause = |index: usize| -> bool {
            let word = words[index].as_str();
            if tokens[index].kind != Kind::Word || !COLUMN_CONSTRAINTS.contains(&word) {
                return false;
            }
            let previous = index.checked_sub(1).map(|index| words[index].as_str());
            // ON DELETE SET NULL, NOT DEFERRABLE, DEFAULT NULL, GENERATED ALWAYS AS
            // and CONSTRAINT name NOT NULL continue the clause before.
            !(matches!(previous, Some("SET" | "DEFAULT" | "ALWAYS" | "NOT"))
                || (word == "NOT" && words.get(index + 1).map(String::as_str) == Some("DEFERRABLE"))
                || index.checked_sub(2).is_some_and(|index| words[index] == "CONSTRAINT"))
        };
        let name = tokens.first().map(|token| item[token.start..token.end].to_string()).unwrap_or_default();
        let mut index = 1;
        let mut type_end = tokens.first().map_or(0, |token| token.end);
        while index < tokens.len() && matches!(tokens[index].kind, Kind::Word | Kind::Name) && !starts_clause(index) {
            type_end = tokens[index].end;
            index += 1;
        }
        if index < tokens.len() && words[index] == "(" && type_end > tokens[0].end {
            if let Some(close) = matching(&tokens, item, index) {
                type_end = tokens[close].end;
                index = close + 1;
            }
        }
        let column_type = item[tokens.first().map_or(0, |token| token.end)..type_end].trim().to_string();

        let mut segments = Vec::new();
        let mut depth = 0usize;
        let mut start = index;
        for position in index..tokens.len() {
            if depth == 0 && position > start && starts_clause(position) {
                segments.push(Self::segment(item, &tokens, &words, start, position));
                start = position;
            }
            match words[position].as_str() {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if start < tokens.len() {
            segments.push(Self::segment(item, &tokens, &words, start, tokens.len()));
        }
        Self { name, column_type, segments, added: Vec::new() }
    }

    fn segment(item: &str, tokens: &[Token], words: &[String], start: usize, end: usize) -> (Segment, String) {
        let keyword = if words[start] == "CONSTRAINT" { start + 2 } else { start };
        let kind = match words.get(keyword).map(String::as_str) {
            Some("NOT") if words.get(keyword + 1).map(String::as_str) == Some("NULL") => Segment::NotNull,
            Some("NULL") => Segment::Null,
            Some("DEFAULT") => Segment::Default,
            Some("UNIQUE") => Segment::Unique,
            Some("REFERENCES") => {
                Segment::References(tokens.get(keyword + 1).map(|token| unquote(&item[token.start..token.end])).unwrap_or_default())
            }
            _ => Segment::Other,
        };
        (kind, item[tokens[start].start..tokens[end - 1].end].to_string())
    }

    fn remove(&mut self, matches: impl Fn(&Segment) -> bool) {
        self.segments.retain(|(segment, _)| !matches(segment));
    }

    fn text(&self) -> String {
        let mut parts = vec![self.name.clone()];
        if !self.column_type.is_empty() {
            parts.push(self.column_type.clone());
        }
        parts.extend(self.segments.iter().map(|(_, text)| text.clone()));
        parts.extend(self.added.iter().cloned());
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_one_column_and_keeps_the_rest() {
        let table = CreateTable::parse(
            "CREATE TABLE [t] (id INTEGER PRIMARY KEY AUTOINCREMENT, \"a b\" NUMERIC(10, 2) NOT NULL DEFAULT 0 CHECK (\"a b\" >= 0), \
             c TEXT COLLATE NOCASE REFERENCES p(id) ON DELETE SET NULL, UNIQUE (c, id)) STRICT",
        )
        .unwrap();
        assert_eq!(table.items.len(), 4);
        assert_eq!(table.options, "STRICT");
        assert!(table.autoincrement);
        assert_eq!(table.column("A B").unwrap(), 1);

        let mut definition = ColumnDefinition::parse(&table.items[1]);
        assert_eq!(definition.column_type, "NUMERIC(10, 2)");
        definition.remove(|segment| matches!(segment, Segment::NotNull | Segment::Default));
        definition.added.push("DEFAULT 1".into());
        assert_eq!(definition.text(), "\"a b\" NUMERIC(10, 2) CHECK (\"a b\" >= 0) DEFAULT 1");

        let definition = ColumnDefinition::parse(&table.items[2]);
        let kinds: Vec<_> = definition.segments.iter().map(|(segment, _)| segment.clone()).collect();
        assert_eq!(kinds, vec![Segment::Other, Segment::References("p".into())]);
        assert_eq!(definition.segments[1].1, "REFERENCES p(id) ON DELETE SET NULL");
    }

    #[test]
    fn removes_constraints_from_items_and_columns() {
        let mut table = CreateTable::parse(
            "CREATE TABLE t (a INT CONSTRAINT one UNIQUE, b INT REFERENCES \"P\", c INT, FOREIGN KEY (c) REFERENCES q (id), CONSTRAINT two UNIQUE (b, c))",
        )
        .unwrap();
        table.remove(&Target::ForeignKey { columns: vec!["c".into()], referenced: "Q".into() }).unwrap();
        table.remove(&Target::ForeignKey { columns: vec!["b".into()], referenced: "p".into() }).unwrap();
        table.remove(&Target::Unique { columns: vec!["a".into()] }).unwrap();
        table.remove(&Target::Unique { columns: vec!["B".into(), "c".into()] }).unwrap();
        assert_eq!(table.items, vec!["a INT", "b INT", "c INT"]);
        assert!(table.remove(&Target::Unique { columns: vec!["a".into()] }).is_err());
    }

    #[test]
    fn finds_foreign_key_names() {
        let names = foreign_key_names(
            "CREATE TABLE t (a INT CONSTRAINT to_p REFERENCES [P] (id), b INT REFERENCES q, c INT, d INT, \
             CONSTRAINT \"two\" FOREIGN KEY (c, d) REFERENCES r (x, y) ON DELETE CASCADE)",
        );
        assert_eq!(
            names,
            vec![(vec!["a".to_string()], "P".to_string(), "to_p".to_string()), (vec!["c".into(), "d".into()], "r".into(), "two".into())]
        );
        let mut table = CreateTable::parse("CREATE TABLE t (a INT CONSTRAINT to_p REFERENCES p, b INT, CONSTRAINT two CHECK (b > 0))").unwrap();
        table.remove(&Target::Named("TO_P".into())).unwrap();
        table.remove(&Target::Named("two".into())).unwrap();
        assert_eq!(table.items, vec!["a INT", "b INT"]);
    }

    #[test]
    fn wraps_expressions_and_qualifies_names() {
        assert_eq!(default_expression("0"), "0");
        assert_eq!(default_expression("-1.5"), "-1.5");
        assert_eq!(default_expression("'x'"), "'x'");
        assert_eq!(default_expression("current_timestamp"), "current_timestamp");
        assert_eq!(default_expression("(1 + 2)"), "(1 + 2)");
        assert_eq!(default_expression("datetime('now')"), "(datetime('now'))");
        assert_eq!(default_expression("(1) + (2)"), "((1) + (2))");
        assert_eq!(qualify_create("CREATE INDEX i ON t (a)", "main"), "CREATE INDEX \"main\".i ON t (a)");
        assert_eq!(qualify_create("CREATE UNIQUE INDEX IF NOT EXISTS [i] ON t (a)", "x"), "CREATE UNIQUE INDEX IF NOT EXISTS \"x\".[i] ON t (a)");
        assert_eq!(qualify_create("CREATE TRIGGER s.g AFTER INSERT ON t BEGIN SELECT 1; END", "main"), "CREATE TRIGGER s.g AFTER INSERT ON t BEGIN SELECT 1; END");
    }
}

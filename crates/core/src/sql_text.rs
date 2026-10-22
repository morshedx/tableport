//! SQL text helpers for the query editor: formatting, and finding the
//! statement under the cursor. Offsets are UTF-16 code units, as Swift's
//! NSString and NSTextView count them.

use sqlformat::{FormatOptions, Indent, QueryParams};

/// A range of the editor's text, in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

/// Beautify: keywords uppercase, two-space indents, a blank line between
/// statements. Comments, string contents and names are kept as written.
#[uniffi::export]
pub fn format_sql(sql: String) -> String {
    // sqlformat's own uppercasing also hits common column names it counts as
    // keywords (status, name, type…), so it keeps case and we uppercase a
    // short list of real keywords ourselves.
    let options = FormatOptions {
        indent: Indent::Spaces(2),
        uppercase: None,
        lines_between_queries: 2,
        ..FormatOptions::default()
    };
    uppercase_keywords(&sqlformat::format(&sql, &QueryParams::None, &options))
}

/// PostgreSQL's reserved words, which can't be bare column names, plus
/// statement words that hardly ever are.
const KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "ANALYZE", "AND", "ANY", "ARRAY", "AS", "ASC", "BEGIN", "BETWEEN", "BOTH", "BY",
    "CASCADE", "CASE", "CAST", "CHECK", "COLLATE", "COLUMN", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE",
    "CROSS", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER", "DEFAULT", "DELETE", "DESC",
    "DISTINCT", "DO", "DROP", "ELSE", "END", "EXCEPT", "EXISTS", "EXPLAIN", "FALSE", "FETCH", "FOR", "FOREIGN",
    "FROM", "FULL", "GRANT", "GROUP", "HAVING", "ILIKE", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO",
    "IS", "JOIN", "LATERAL", "LEFT", "LIKE", "LIMIT", "NOT", "NOTHING", "NULL", "NULLS", "OFFSET", "ON", "ONLY",
    "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY", "REFERENCES", "RETURNING", "RIGHT", "ROLLBACK",
    "SELECT", "SET", "SOME", "TABLE", "THEN", "TO", "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USING",
    "VALUES", "VIEW", "WHEN", "WHERE", "WINDOW", "WITH",
];

/// Uppercases KEYWORDS outside quotes, comments and dollar-quoted bodies.
/// A word after `.` is a qualified name, not a keyword.
fn uppercase_keywords(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        match chars[i] {
            '\'' | '"' => i = skip_quoted(&chars, i, chars[i]),
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            '$' => i = skip_dollar_quoted(&chars, i),
            c if c.is_alphabetic() || c == '_' => {
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let upper = word.to_ascii_uppercase();
                let qualified = start > 0 && chars[start - 1] == '.';
                if !qualified && KEYWORDS.contains(&upper.as_str()) {
                    out.push_str(&upper);
                } else {
                    out.push_str(&word);
                }
                continue;
            }
            _ => i += 1,
        }
        out.extend(&chars[start..i.min(chars.len())]);
    }
    out
}

/// The first word of each statement, uppercase ("SELECT", "UPDATE"), in the
/// same order and count as `split`.
pub(crate) fn statement_verbs(sql: &str) -> Vec<String> {
    let units: Vec<u16> = sql.encode_utf16().collect();
    split(sql)
        .into_iter()
        .map(|(start, end)| {
            let text = String::from_utf16_lossy(&units[start..end]);
            let mut rest = text.as_str();
            loop {
                rest = rest.trim_start();
                if let Some(after) = rest.strip_prefix("--") {
                    rest = after.split_once('\n').map_or("", |(_, next)| next);
                } else if let Some(after) = rest.strip_prefix("/*") {
                    rest = after.split_once("*/").map_or("", |(_, next)| next);
                } else {
                    break;
                }
            }
            rest.chars()
                .take_while(|c| c.is_alphabetic() || *c == '_')
                .collect::<String>()
                .to_ascii_uppercase()
        })
        .collect()
}

/// The text of each statement, in the same order and count as `split`.
pub(crate) fn statements(sql: &str) -> Vec<String> {
    let units: Vec<u16> = sql.encode_utf16().collect();
    split(sql).into_iter().map(|(start, end)| String::from_utf16_lossy(&units[start..end])).collect()
}

/// The statement around `cursor`, without its `;` and surrounding blank
/// space. A cursor just after a `;` (or on the blank lines after it) belongs
/// to the statement before. Semicolons inside quotes, dollar-quoted bodies
/// and comments don't end a statement. None when there is only blank text.
#[uniffi::export]
pub fn statement_range(sql: String, cursor: u32) -> Option<TextRange> {
    let units: Vec<u16> = sql.encode_utf16().collect();
    let cursor = (cursor as usize).min(units.len());
    let statements = split(&sql);
    // The statement that contains the cursor, else the last one that starts
    // before it.
    let chosen = statements
        .iter()
        .find(|(start, end)| *start <= cursor && cursor <= *end)
        .or_else(|| statements.iter().rev().find(|(start, _)| *start <= cursor))
        .or_else(|| statements.first())?;
    Some(TextRange { start: chosen.0 as u32, end: chosen.1 as u32 })
}

/// Each statement's trimmed range in UTF-16 units, empty ones left out.
fn split(sql: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = sql.chars().collect();
    // UTF-16 offset before each char, plus the end.
    let mut offsets = Vec::with_capacity(chars.len() + 1);
    let mut unit = 0;
    for c in &chars {
        offsets.push(unit);
        unit += c.len_utf16();
    }
    offsets.push(unit);

    let mut statements = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => i = skip_quoted(&chars, i, chars[i]),
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '$' => i = skip_dollar_quoted(&chars, i),
            ';' => {
                push_trimmed(&chars, &offsets, start, i, &mut statements);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    push_trimmed(&chars, &offsets, start, chars.len(), &mut statements);
    statements
}

fn push_trimmed(chars: &[char], offsets: &[usize], mut start: usize, mut end: usize, out: &mut Vec<(usize, usize)>) {
    end = end.min(chars.len());
    while start < end && chars[start].is_whitespace() {
        start += 1;
    }
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if start < end {
        out.push((offsets[start], offsets[end]));
    }
}

/// Index after the closing quote; a doubled quote stays inside.
fn skip_quoted(chars: &[char], open: usize, quote: char) -> usize {
    let mut i = open + 1;
    while i < chars.len() {
        if chars[i] == quote {
            if chars.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

/// `$tag$ … $tag$` bodies; a lone `$` (as in `$1`) is skipped as one char.
fn skip_dollar_quoted(chars: &[char], open: usize) -> usize {
    let mut tag_end = open + 1;
    while tag_end < chars.len() && (chars[tag_end].is_alphanumeric() || chars[tag_end] == '_') {
        tag_end += 1;
    }
    if chars.get(tag_end) != Some(&'$') || chars[open + 1..tag_end].first().is_some_and(|c| c.is_ascii_digit()) {
        return open + 1;
    }
    let tag: Vec<char> = chars[open..=tag_end].to_vec();
    let mut i = tag_end + 1;
    while i + tag.len() <= chars.len() {
        if chars[i..i + tag.len()] == tag[..] {
            return i + tag.len();
        }
        i += 1;
    }
    chars.len()
}

/// Format JSON in the cell editor: two-space indents, keys in their
/// original order, numbers exactly as written. None when the text is not
/// JSON, so the editor can say so.
#[uniffi::export]
pub fn format_json(text: String) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

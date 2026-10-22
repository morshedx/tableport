use tableport_core::{format_json, format_sql, statement_range, TextRange};

fn current(sql: &str, cursor: usize) -> Option<String> {
    let units: Vec<u16> = sql.encode_utf16().collect();
    statement_range(sql.to_string(), cursor as u32)
        .map(|TextRange { start, end }| String::from_utf16(&units[start as usize..end as usize]).unwrap())
}

#[test]
fn finds_the_statement_under_the_cursor() {
    let sql = "SELECT 1;\n\nSELECT 'a;b' AS x;\nSELECT 3";
    assert_eq!(current(sql, 0).as_deref(), Some("SELECT 1"));
    assert_eq!(current(sql, 9).as_deref(), Some("SELECT 1"), "just after the semicolon");
    assert_eq!(current(sql, 15).as_deref(), Some("SELECT 'a;b' AS x"), "the ; in quotes doesn't split");
    assert_eq!(current(sql, sql.len()).as_deref(), Some("SELECT 3"));
}

#[test]
fn comments_and_dollar_quotes_dont_split() {
    let sql = "-- first; still a comment\nSELECT 1 /* ; */ AS a;\nDO $body$ BEGIN PERFORM 1; END $body$;\nSELECT $1";
    assert_eq!(current(sql, 30).as_deref(), Some("-- first; still a comment\nSELECT 1 /* ; */ AS a"));
    let body = sql.find("PERFORM").unwrap();
    assert_eq!(current(sql, body).as_deref(), Some("DO $body$ BEGIN PERFORM 1; END $body$"));
    assert_eq!(current(sql, sql.len()).as_deref(), Some("SELECT $1"));
}

#[test]
fn offsets_are_utf16() {
    // "😀" is two UTF-16 units: the second statement starts at unit 15.
    let sql = "SELECT '😀';\nSELECT 2";
    assert_eq!(current(sql, 16).as_deref(), Some("SELECT 2"));
}

#[test]
fn blank_text_has_no_statement() {
    assert_eq!(statement_range("  \n ;; ".into(), 2), None);
}

#[test]
fn beautify_uppercases_and_indents() {
    let formatted = format_sql("select id, name from users where id = 1 and name = 'x'".into());
    assert!(formatted.starts_with("SELECT\n  id,\n  name\nFROM\n  users\nWHERE"), "{formatted}");
    assert!(formatted.contains("'x'"), "{formatted}");
}

#[test]
fn beautify_keeps_column_names_as_written() {
    let formatted = format_sql("update reports set status = 'draft' where id in ('a') and t.order = 1".into());
    assert!(formatted.contains("status = 'draft'"), "{formatted}");
    assert!(formatted.starts_with("UPDATE\n  reports\nSET"), "{formatted}");
    assert!(formatted.contains("t.order"), "{formatted}");
    assert!(formatted.contains("WHERE"), "{formatted}");
}

#[test]
fn json_keeps_key_order_and_exact_numbers() {
    let formatted = format_json(r#"{"z":1,"a":[true,null],"big":12345678901234567890.123456789}"#.into()).unwrap();
    assert_eq!(formatted, "{\n  \"z\": 1,\n  \"a\": [\n    true,\n    null\n  ],\n  \"big\": 12345678901234567890.123456789\n}");
    assert_eq!(format_json("not json".into()), None);
}

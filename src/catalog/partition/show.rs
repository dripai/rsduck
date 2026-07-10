use super::*;
use crate::db::{SqlColumn, SqlType, SqlTypedResult, SqlValue};

pub(crate) fn looks_like_show_partitions(sql: &str) -> bool {
    normalize_show_partitions(sql).starts_with("show partitions from ")
}

pub(crate) fn show_partitions_result(
    conn: &Connection,
    username: &str,
    sql: &str,
) -> Result<Option<SqlTypedResult>, String> {
    if !looks_like_show_partitions(sql) {
        return Ok(None);
    }
    let (schema, table) = parse_show_partitions(sql)?;
    reject_reserved_schema(&schema)?;
    let principal = principal_for_username(conn, username)?;
    require_relation_action(conn, &principal, &(schema.clone(), table.clone()), "read")?;
    let parent_oid = relation_oid(conn, &schema, &table)?;
    let relkind = relation_kind(conn, parent_oid)?;
    if relkind != "p" {
        return Err(format!(
            "SHOW PARTITIONS requires a partitioned table: {schema}.{table}"
        ));
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT
               pn.nspname AS schema_name,
               parent.relname AS table_name,
               p.partition_value,
               cn.nspname AS physical_schema,
               child.relname AS physical_table,
               p.status,
               p.row_count,
               p.is_null_partition,
               CAST(p.created_at AS VARCHAR) AS created_at,
               CAST(p.activated_at AS VARCHAR) AS activated_at,
               CAST(p.dropped_at AS VARCHAR) AS dropped_at,
               p.error_message
             FROM rsduck_catalog.rs_partition p
             JOIN rsduck_catalog.rs_relation parent ON parent.oid = p.parent_relid
             JOIN rsduck_catalog.rs_schema pn ON pn.oid = parent.relnamespace
             JOIN rsduck_catalog.rs_relation child ON child.oid = p.child_relid
             JOIN rsduck_catalog.rs_schema cn ON cn.oid = child.relnamespace
             WHERE p.parent_relid = {parent_oid}
             ORDER BY p.is_null_partition, p.partition_value"
        ))
        .map_err(|e| format!("prepare SHOW PARTITIONS failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(vec![
                SqlValue::Text(row.get::<_, String>(0)?),
                SqlValue::Text(row.get::<_, String>(1)?),
                SqlValue::Text(row.get::<_, String>(2)?),
                SqlValue::Text(row.get::<_, String>(3)?),
                SqlValue::Text(row.get::<_, String>(4)?),
                SqlValue::Text(row.get::<_, String>(5)?),
                SqlValue::Int64(row.get::<_, i64>(6)?),
                SqlValue::Bool(row.get::<_, bool>(7)?),
                SqlValue::Text(row.get::<_, String>(8)?),
                row.get::<_, Option<String>>(9)?
                    .map(SqlValue::Text)
                    .unwrap_or(SqlValue::Null),
                row.get::<_, Option<String>>(10)?
                    .map(SqlValue::Text)
                    .unwrap_or(SqlValue::Null),
                row.get::<_, Option<String>>(11)?
                    .map(SqlValue::Text)
                    .unwrap_or(SqlValue::Null),
            ])
        })
        .map_err(|e| format!("query SHOW PARTITIONS failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read SHOW PARTITIONS failed: {e}"))?;

    Ok(Some(SqlTypedResult::Query {
        columns: vec![
            sql_column("schema_name", SqlType::Text),
            sql_column("table_name", SqlType::Text),
            sql_column("partition_value", SqlType::Text),
            sql_column("physical_schema", SqlType::Text),
            sql_column("physical_table", SqlType::Text),
            sql_column("status", SqlType::Text),
            sql_column("row_count", SqlType::Int8),
            sql_column("is_null_partition", SqlType::Bool),
            sql_column("created_at", SqlType::Text),
            sql_column("activated_at", SqlType::Text),
            sql_column("dropped_at", SqlType::Text),
            sql_column("error_message", SqlType::Text),
        ],
        rows,
    }))
}

fn sql_column(name: &str, data_type: SqlType) -> SqlColumn {
    SqlColumn {
        name: name.to_string(),
        data_type,
    }
}

fn parse_show_partitions(sql: &str) -> Result<(String, String), String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let prefix = "show partitions from";
    if !normalize_show_partitions(trimmed).starts_with(prefix) {
        return Err("SHOW PARTITIONS syntax: SHOW PARTITIONS FROM table_name".into());
    }
    let mut cursor = skip_ascii_ws(trimmed, prefix.len());
    let mut parts = Vec::new();
    loop {
        let (part, next) = parse_identifier_part(trimmed, cursor)?;
        parts.push(part);
        cursor = skip_ascii_ws(trimmed, next);
        if cursor >= trimmed.len() || trimmed.as_bytes()[cursor] != b'.' {
            break;
        }
        cursor = skip_ascii_ws(trimmed, cursor + 1);
    }
    let trailing = trimmed[cursor..].trim();
    if !trailing.is_empty() {
        return Err(format!(
            "unexpected text after SHOW PARTITIONS table name: {trailing}"
        ));
    }
    match parts.as_slice() {
        [table] => Ok(("main".to_string(), table.clone())),
        [schema, table] => Ok((schema.clone(), table.clone())),
        _ => Err("SHOW PARTITIONS only supports table or schema.table".into()),
    }
}

fn normalize_show_partitions(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn skip_ascii_ws(sql: &str, mut cursor: usize) -> usize {
    while cursor < sql.len() && sql.as_bytes()[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn parse_identifier_part(sql: &str, cursor: usize) -> Result<(String, usize), String> {
    let cursor = skip_ascii_ws(sql, cursor);
    if cursor >= sql.len() {
        return Err("expected identifier".into());
    }
    match sql.as_bytes()[cursor] {
        b'"' => parse_quoted_identifier(sql, cursor, '"'),
        b'`' => parse_quoted_identifier(sql, cursor, '`'),
        byte if byte.is_ascii_alphanumeric() || byte == b'_' => {
            let start = cursor;
            let mut end = cursor + 1;
            while end < sql.len()
                && (sql.as_bytes()[end].is_ascii_alphanumeric() || sql.as_bytes()[end] == b'_')
            {
                end += 1;
            }
            Ok((sql[start..end].to_string(), end))
        }
        _ => Err(format!("expected identifier at: {}", sql[cursor..].trim())),
    }
}

fn parse_quoted_identifier(
    sql: &str,
    cursor: usize,
    quote: char,
) -> Result<(String, usize), String> {
    let mut out = String::new();
    let mut idx = cursor + quote.len_utf8();
    while idx < sql.len() {
        let ch = sql[idx..]
            .chars()
            .next()
            .ok_or_else(|| "invalid identifier".to_string())?;
        if ch == quote {
            let next = idx + quote.len_utf8();
            if next < sql.len() && sql[next..].starts_with(quote) {
                out.push(quote);
                idx = next + quote.len_utf8();
                continue;
            }
            return Ok((out, next));
        }
        out.push(ch);
        idx += ch.len_utf8();
    }
    Err("unterminated quoted identifier".into())
}

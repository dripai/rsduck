use tokio::net::TcpStream;
use tracing::debug;

use chrono::{Datelike, Timelike};

use crate::db::{DbHandle, SqlColumn, SqlParam, SqlType, SqlTypedResult, SqlValue};

use super::codec::{
    eof_packet, err_packet, get_u32_le, ok_packet, ok_packet_with_rows, put_lenenc_bytes,
    put_lenenc_int, put_lenenc_str, put_u16_le, put_u32_le, read_packet, take, write_packet,
};
use super::session::{MySqlSession, PreparedStatement};
use super::stmt::{parse_execute_params, rewrite_mysql_placeholders};
use super::types::*;

pub(super) async fn command_loop(
    stream: &mut TcpStream,
    db: DbHandle,
    mut session: MySqlSession,
) -> Result<(), String> {
    loop {
        let packet = read_packet(stream).await?;
        if packet.payload.is_empty() {
            continue;
        }
        let command = packet.payload[0];
        let payload = &packet.payload[1..];
        let mut sequence = 1_u8;
        match command {
            COM_QUIT => return Ok(()),
            COM_PING => write_packet(stream, &mut sequence, &ok_packet()).await?,
            COM_INIT_DB => {
                session.database = String::from_utf8(payload.to_vec())
                    .map_err(|e| format!("invalid database name: {e}"))?;
                write_packet(stream, &mut sequence, &ok_packet()).await?;
            }
            COM_QUERY => handle_query(stream, &mut sequence, &db, &session, payload).await?,
            COM_STMT_PREPARE => {
                handle_stmt_prepare(stream, &mut sequence, &db, &mut session, payload).await?
            }
            COM_STMT_EXECUTE => {
                handle_stmt_execute(stream, &mut sequence, &db, &mut session, payload).await?
            }
            COM_STMT_CLOSE => {
                let mut idx = 0;
                let statement_id = get_u32_le(payload, &mut idx)?;
                session.statements.remove(&statement_id);
            }
            COM_STMT_RESET => {
                let mut idx = 0;
                let statement_id = get_u32_le(payload, &mut idx)?;
                if let Some(stmt) = session.statements.get_mut(&statement_id) {
                    stmt.param_types.clear();
                }
                write_packet(stream, &mut sequence, &ok_packet()).await?;
            }
            COM_STMT_SEND_LONG_DATA => {
                write_packet(
                    stream,
                    &mut sequence,
                    &err_packet(1235, "0A000", "COM_STMT_SEND_LONG_DATA is not supported"),
                )
                .await?;
            }
            other => {
                write_packet(
                    stream,
                    &mut sequence,
                    &err_packet(
                        1047,
                        "08S01",
                        &format!("unsupported MySQL command: {other}"),
                    ),
                )
                .await?;
            }
        }
    }
}

async fn handle_query(
    stream: &mut TcpStream,
    sequence: &mut u8,
    db: &DbHandle,
    session: &MySqlSession,
    payload: &[u8],
) -> Result<(), String> {
    let sql = std::str::from_utf8(payload)
        .map_err(|e| format!("invalid COM_QUERY SQL UTF-8: {e}"))?
        .trim()
        .to_string();
    debug!(target: "rsduck::mysql", user = %session.username, sql = %sql, "MySQL query");

    if is_mysql_session_statement(&sql) {
        write_packet(stream, sequence, &ok_packet()).await?;
        return Ok(());
    }

    let result = if let Some(result) = mysql_compat_result(&sql, session) {
        Ok(result)
    } else {
        let execution_sql = mysql_execution_sql(&sql, session);
        db.execute_typed_sql_as(session.username.clone(), execution_sql)
            .await
            .map_err(|e| e.to_string())
    };
    send_result(stream, sequence, result, false).await
}

async fn handle_stmt_prepare(
    stream: &mut TcpStream,
    sequence: &mut u8,
    db: &DbHandle,
    session: &mut MySqlSession,
    payload: &[u8],
) -> Result<(), String> {
    let original_sql = std::str::from_utf8(payload)
        .map_err(|e| format!("invalid COM_STMT_PREPARE SQL UTF-8: {e}"))?
        .trim()
        .to_string();
    let (bound_sql, param_count) = rewrite_mysql_placeholders(&original_sql)?;
    let bound_sql = mysql_execution_sql(&bound_sql, session);
    let describe_params = dummy_params_for_describe(&bound_sql, param_count);
    let columns = db
        .describe_sql_with_params_as(session.username.clone(), bound_sql.clone(), describe_params)
        .await
        .map_err(|e| e.to_string())?;

    let statement_id = session.next_statement_id();
    let stmt = PreparedStatement {
        bound_sql,
        param_count,
        param_types: Vec::new(),
        columns: columns.clone(),
    };
    session.statements.insert(statement_id, stmt);

    let mut packet = Vec::new();
    packet.push(0x00);
    put_u32_le(&mut packet, statement_id);
    put_u16_le(&mut packet, columns.len() as u16);
    put_u16_le(&mut packet, param_count as u16);
    packet.push(0x00);
    put_u16_le(&mut packet, 0);
    write_packet(stream, sequence, &packet).await?;

    for idx in 0..param_count {
        write_packet(stream, sequence, &param_column_definition(idx)).await?;
    }
    if param_count > 0 {
        write_packet(stream, sequence, &eof_packet()).await?;
    }
    for column in columns {
        write_packet(
            stream,
            sequence,
            &column_definition(&session.database, &column),
        )
        .await?;
    }
    if !session
        .statements
        .get(&statement_id)
        .is_some_and(|stmt| stmt.columns.is_empty())
    {
        write_packet(stream, sequence, &eof_packet()).await?;
    }
    Ok(())
}

async fn handle_stmt_execute(
    stream: &mut TcpStream,
    sequence: &mut u8,
    db: &DbHandle,
    session: &mut MySqlSession,
    payload: &[u8],
) -> Result<(), String> {
    let mut idx = 0;
    let statement_id = get_u32_le(payload, &mut idx)?;
    let _flags = *take(payload, &mut idx, 1)?
        .first()
        .ok_or_else(|| "missing execute flags".to_string())?;
    let _iteration_count = get_u32_le(payload, &mut idx)?;
    let stmt = session
        .statements
        .get_mut(&statement_id)
        .ok_or_else(|| format!("unknown prepared statement id: {statement_id}"))?;
    let (params, param_types) =
        parse_execute_params(payload, &mut idx, stmt.param_count, &stmt.param_types)?;
    stmt.param_types = param_types;

    let result = db
        .execute_typed_sql_with_params_as(session.username.clone(), stmt.bound_sql.clone(), params)
        .await
        .map_err(|e| e.to_string());
    send_result(stream, sequence, result, true).await
}

async fn send_result(
    stream: &mut TcpStream,
    sequence: &mut u8,
    result: Result<SqlTypedResult, String>,
    binary: bool,
) -> Result<(), String> {
    match result {
        Ok(SqlTypedResult::Query { columns, rows }) => {
            write_packet(stream, sequence, &resultset_header(columns.len())).await?;
            for column in &columns {
                write_packet(stream, sequence, &column_definition("memory", column)).await?;
            }
            write_packet(stream, sequence, &eof_packet()).await?;
            for row in rows {
                let payload = if binary {
                    binary_row(&row)
                } else {
                    text_row(&row)
                };
                write_packet(stream, sequence, &payload).await?;
            }
            write_packet(stream, sequence, &eof_packet()).await?;
        }
        Ok(SqlTypedResult::Execute { affected_rows, .. }) => {
            write_packet(stream, sequence, &ok_packet_with_rows(affected_rows)).await?;
        }
        Err(e) => {
            write_packet(stream, sequence, &err_packet(1105, "HY000", &e)).await?;
        }
    }
    Ok(())
}

fn resultset_header(column_count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    put_lenenc_int(&mut out, column_count as u64);
    out
}

fn column_definition(database: &str, column: &SqlColumn) -> Vec<u8> {
    let (column_type, charset, flags) = mysql_type_for_sql_type(column.data_type);
    let mut out = Vec::new();
    put_lenenc_str(&mut out, "def");
    put_lenenc_str(&mut out, database);
    put_lenenc_str(&mut out, "");
    put_lenenc_str(&mut out, "");
    put_lenenc_str(&mut out, &column.name);
    put_lenenc_str(&mut out, &column.name);
    out.push(0x0c);
    put_u16_le(&mut out, charset);
    put_u32_le(&mut out, 1024 * 1024);
    out.push(column_type);
    put_u16_le(&mut out, flags);
    out.push(decimals_for_type(column.data_type));
    out.extend_from_slice(&[0_u8; 2]);
    out
}

fn param_column_definition(idx: usize) -> Vec<u8> {
    column_definition(
        "",
        &SqlColumn {
            name: format!("?{}", idx + 1),
            data_type: SqlType::Text,
        },
    )
}

fn text_row(row: &[SqlValue]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in row {
        if matches!(value, SqlValue::Null) {
            out.push(0xfb);
        } else {
            put_lenenc_str(&mut out, &value.text_value().unwrap_or_default());
        }
    }
    out
}

fn binary_row(row: &[SqlValue]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x00);
    let null_bitmap_len = (row.len() + 7 + 2) / 8;
    let null_bitmap_start = out.len();
    out.resize(out.len() + null_bitmap_len, 0);
    for (idx, value) in row.iter().enumerate() {
        if matches!(value, SqlValue::Null) {
            let bit = idx + 2;
            out[null_bitmap_start + bit / 8] |= 1 << (bit % 8);
        } else {
            put_binary_value(&mut out, value);
        }
    }
    out
}

fn put_binary_value(out: &mut Vec<u8>, value: &SqlValue) {
    match value {
        SqlValue::Null => {}
        SqlValue::Bool(value) => out.push(u8::from(*value)),
        SqlValue::Int16(value) => out.extend_from_slice(&value.to_le_bytes()),
        SqlValue::Int32(value) => out.extend_from_slice(&value.to_le_bytes()),
        SqlValue::Int64(value) => out.extend_from_slice(&value.to_le_bytes()),
        SqlValue::Float32(value) => out.extend_from_slice(&value.to_le_bytes()),
        SqlValue::Float64(value) => out.extend_from_slice(&value.to_le_bytes()),
        SqlValue::Date(value) => put_binary_date(out, *value),
        SqlValue::Time(value) => put_binary_time(out, *value),
        SqlValue::Timestamp(value) => put_binary_datetime(out, *value),
        SqlValue::TimestampTz(value) => put_binary_datetime(out, value.naive_utc()),
        SqlValue::Bytes(value) => put_lenenc_bytes(out, value),
        SqlValue::Decimal(value) => put_lenenc_str(out, &value.to_string()),
        SqlValue::NumericText(value) | SqlValue::Text(value) => put_lenenc_str(out, value),
        SqlValue::Uuid(value) => put_lenenc_str(out, &value.to_string()),
        SqlValue::Json(value) => put_lenenc_str(out, &value.to_string()),
        SqlValue::Interval { .. } => put_lenenc_str(out, &value.text_value().unwrap_or_default()),
    }
}

fn put_binary_date(out: &mut Vec<u8>, value: chrono::NaiveDate) {
    out.push(4);
    put_u16_le(out, value.year() as u16);
    out.push(value.month() as u8);
    out.push(value.day() as u8);
}

fn put_binary_datetime(out: &mut Vec<u8>, value: chrono::NaiveDateTime) {
    let micros = value.and_utc().timestamp_subsec_micros();
    if micros == 0 {
        out.push(7);
    } else {
        out.push(11);
    }
    put_u16_le(out, value.year() as u16);
    out.push(value.month() as u8);
    out.push(value.day() as u8);
    out.push(value.hour() as u8);
    out.push(value.minute() as u8);
    out.push(value.second() as u8);
    if micros != 0 {
        put_u32_le(out, micros);
    }
}

fn put_binary_time(out: &mut Vec<u8>, value: chrono::NaiveTime) {
    let micros = value.nanosecond() / 1_000;
    if micros == 0 {
        out.push(8);
    } else {
        out.push(12);
    }
    out.push(0);
    put_u32_le(out, 0);
    out.push(value.hour() as u8);
    out.push(value.minute() as u8);
    out.push(value.second() as u8);
    if micros != 0 {
        put_u32_le(out, micros);
    }
}

fn decimals_for_type(data_type: SqlType) -> u8 {
    match data_type {
        SqlType::Float4 | SqlType::Float8 | SqlType::Numeric => 31,
        SqlType::Timestamp | SqlType::TimestampTz | SqlType::Time => 6,
        _ => 0,
    }
}

fn is_mysql_session_statement(sql: &str) -> bool {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    normalized.starts_with("set ")
        || normalized == "begin"
        || normalized == "commit"
        || normalized == "rollback"
}

fn mysql_compat_result(sql: &str, session: &MySqlSession) -> Option<SqlTypedResult> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    if references_information_schema_engines(sql) || normalized == "show engines" {
        return Some(mysql_engines_result());
    }
    if let Some(pattern) = show_variables_like_pattern(sql) {
        return Some(system_variables_result(Some(&pattern)));
    }
    if normalized == "show variables" {
        return Some(system_variables_result(None));
    }
    if normalized == "select database()" {
        return Some(single_text_row("DATABASE()", &session.database));
    }
    if normalized == "select @@version_comment" {
        return Some(single_text_row(
            "@@version_comment",
            "rsduck MySQL protocol",
        ));
    }
    if normalized == "select version()" {
        return Some(single_text_row("version()", "8.0.34-rsduck"));
    }
    None
}

fn mysql_catalog_query_sql(sql: &str, session: &MySqlSession) -> Option<String> {
    crate::mysql_compat::rewrite_sql(sql, current_mysql_schema(session), &session.username)
}

fn mysql_execution_sql(sql: &str, session: &MySqlSession) -> String {
    let sql = mysql_catalog_query_sql(sql, session)
        .or_else(|| rewrite_mysql_limit_offset_comma(sql))
        .unwrap_or_else(|| sql.to_string());
    rewrite_mysql_quoted_identifiers(&sql)
}

fn rewrite_mysql_quoted_identifiers(sql: &str) -> String {
    let mut output = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut changed = false;

    while idx < sql.len() {
        let byte = sql.as_bytes()[idx];
        match byte {
            b'`' => {
                idx += 1;
                output.push('"');
                let mut closed = false;
                while idx < sql.len() {
                    match sql.as_bytes()[idx] {
                        b'`' if sql.as_bytes().get(idx + 1) == Some(&b'`') => {
                            output.push('`');
                            idx += 2;
                        }
                        b'`' => {
                            output.push('"');
                            idx += 1;
                            closed = true;
                            break;
                        }
                        _ => {
                            let next = next_sql_char_index(sql, idx);
                            output.push_str(&sql[idx..next]);
                            idx = next;
                        }
                    }
                }
                if !closed {
                    return sql.to_string();
                }
                changed = true;
            }
            b'\'' | b'"' => {
                let quote = byte;
                let start = idx;
                idx += 1;
                while idx < sql.len() {
                    if sql.as_bytes()[idx] == quote {
                        idx += 1;
                        if sql.as_bytes().get(idx) == Some(&quote) {
                            idx += 1;
                            continue;
                        }
                        break;
                    }
                    idx = next_sql_char_index(sql, idx);
                }
                output.push_str(&sql[start..idx]);
            }
            _ => {
                let next = next_sql_char_index(sql, idx);
                output.push_str(&sql[idx..next]);
                idx = next;
            }
        }
    }

    if changed {
        output
    } else {
        sql.to_string()
    }
}

fn rewrite_mysql_limit_offset_comma(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len() + 16);
    let mut idx = 0;
    let mut last = 0;
    let mut replaced = false;

    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' => {
                idx = skip_sql_quoted(sql, idx, b'\'', true);
                continue;
            }
            b'"' => {
                idx = skip_sql_quoted(sql, idx, b'"', false);
                continue;
            }
            b'`' => {
                idx = skip_sql_quoted(sql, idx, b'`', false);
                continue;
            }
            b'-' if bytes.get(idx + 1) == Some(&b'-') => {
                idx = skip_line_comment(sql, idx + 2);
                continue;
            }
            b'#' => {
                idx = skip_line_comment(sql, idx + 1);
                continue;
            }
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                idx = skip_block_comment(sql, idx + 2);
                continue;
            }
            _ => {}
        }

        if !sql.is_char_boundary(idx) {
            idx += 1;
            continue;
        }

        if consume_keyword(sql, idx, "limit").is_some() {
            let terms_start = skip_mysql_space(sql, idx + "limit".len());
            if let Some((offset_start, offset_end, count_start, count_end)) =
                parse_mysql_limit_comma_terms(sql, terms_start)
            {
                output.push_str(&sql[last..idx]);
                output.push_str("LIMIT ");
                output.push_str(sql[count_start..count_end].trim());
                output.push_str(" OFFSET ");
                output.push_str(sql[offset_start..offset_end].trim());
                last = count_end;
                idx = count_end;
                replaced = true;
                continue;
            }
        }

        idx += 1;
    }

    if replaced {
        output.push_str(&sql[last..]);
        Some(output)
    } else {
        None
    }
}

fn parse_mysql_limit_comma_terms(sql: &str, start: usize) -> Option<(usize, usize, usize, usize)> {
    let offset_start = skip_mysql_space(sql, start);
    let comma_idx = find_limit_comma(sql, offset_start)?;
    let offset_end = trim_ascii_space_end(sql, offset_start, comma_idx);
    let count_start = skip_mysql_space(sql, comma_idx + 1);
    let count_end = trim_ascii_space_end(sql, count_start, find_limit_count_end(sql, count_start));
    if offset_start >= offset_end || count_start >= count_end {
        return None;
    }
    Some((offset_start, offset_end, count_start, count_end))
}

fn find_limit_comma(sql: &str, start: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut idx = start;
    let mut depth = 0_usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' => idx = skip_sql_quoted(sql, idx, b'\'', true),
            b'"' => idx = skip_sql_quoted(sql, idx, b'"', false),
            b'`' => idx = skip_sql_quoted(sql, idx, b'`', false),
            b'-' if bytes.get(idx + 1) == Some(&b'-') => idx = skip_line_comment(sql, idx + 2),
            b'#' => idx = skip_line_comment(sql, idx + 1),
            b'/' if bytes.get(idx + 1) == Some(&b'*') => idx = skip_block_comment(sql, idx + 2),
            b'(' => {
                depth += 1;
                idx += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                idx += 1;
            }
            b',' if depth == 0 => return Some(idx),
            b';' if depth == 0 => return None,
            _ => idx += 1,
        }
    }
    None
}

fn find_limit_count_end(sql: &str, start: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut idx = start;
    let mut depth = 0_usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' => idx = skip_sql_quoted(sql, idx, b'\'', true),
            b'"' => idx = skip_sql_quoted(sql, idx, b'"', false),
            b'`' => idx = skip_sql_quoted(sql, idx, b'`', false),
            b'-' if bytes.get(idx + 1) == Some(&b'-') && depth == 0 => return idx,
            b'#' if depth == 0 => return idx,
            b'/' if bytes.get(idx + 1) == Some(&b'*') && depth == 0 => return idx,
            b'-' if bytes.get(idx + 1) == Some(&b'-') => idx = skip_line_comment(sql, idx + 2),
            b'#' => idx = skip_line_comment(sql, idx + 1),
            b'/' if bytes.get(idx + 1) == Some(&b'*') => idx = skip_block_comment(sql, idx + 2),
            b'(' => {
                depth += 1;
                idx += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                idx += 1;
            }
            b';' if depth == 0 => return idx,
            byte if depth == 0 && byte.is_ascii_whitespace() => {
                let next_idx = skip_mysql_space(sql, idx);
                if is_limit_tail_keyword(sql, next_idx) {
                    return idx;
                }
                idx = next_idx;
            }
            _ => idx += 1,
        }
    }
    idx
}

fn is_limit_tail_keyword(sql: &str, idx: usize) -> bool {
    [
        "for",
        "lock",
        "procedure",
        "into",
        "union",
        "except",
        "intersect",
    ]
    .iter()
    .any(|keyword| consume_keyword(sql, idx, keyword).is_some())
}

fn trim_ascii_space_end(sql: &str, start: usize, mut end: usize) -> usize {
    let bytes = sql.as_bytes();
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn skip_sql_quoted(sql: &str, start: usize, quote: u8, backslash_escape: bool) -> usize {
    let bytes = sql.as_bytes();
    let mut idx = start + 1;
    while idx < bytes.len() {
        if backslash_escape && bytes[idx] == b'\\' {
            idx = (idx + 2).min(bytes.len());
            continue;
        }
        if bytes[idx] == quote {
            if bytes.get(idx + 1) == Some(&quote) {
                idx += 2;
                continue;
            }
            return idx + 1;
        }
        idx += 1;
    }
    bytes.len()
}

fn skip_line_comment(sql: &str, start: usize) -> usize {
    sql.as_bytes()[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(sql.len(), |pos| start + pos + 1)
}

fn skip_block_comment(sql: &str, start: usize) -> usize {
    sql.as_bytes()[start..]
        .windows(2)
        .position(|window| window == b"*/")
        .map_or(sql.len(), |pos| start + pos + 2)
}

fn current_mysql_schema(session: &MySqlSession) -> &str {
    if session.database.is_empty() || session.database.eq_ignore_ascii_case("memory") {
        "main"
    } else {
        &session.database
    }
}

fn consume_keyword(sql: &str, idx: usize, keyword: &str) -> Option<usize> {
    let end = idx.checked_add(keyword.len())?;
    if end <= sql.len()
        && sql[idx..end].eq_ignore_ascii_case(keyword)
        && (idx == 0 || !is_mysql_ident_byte(sql.as_bytes()[idx - 1]))
        && (end == sql.len() || !is_mysql_ident_byte(sql.as_bytes()[end]))
    {
        Some(end)
    } else {
        None
    }
}

fn skip_mysql_space(sql: &str, mut idx: usize) -> usize {
    while sql
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        idx += 1;
    }
    idx
}

fn is_mysql_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn references_information_schema_engines(sql: &str) -> bool {
    sql.chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '`' && *ch != '"')
        .collect::<String>()
        .to_ascii_lowercase()
        .contains("information_schema.engines")
}

fn mysql_engines_result() -> SqlTypedResult {
    SqlTypedResult::Query {
        columns: vec![
            SqlColumn {
                name: "ENGINE".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "SUPPORT".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "COMMENT".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "TRANSACTIONS".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "XA".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "SAVEPOINTS".to_string(),
                data_type: SqlType::Text,
            },
        ],
        rows: vec![vec![
            SqlValue::Text("InnoDB".to_string()),
            SqlValue::Text("DEFAULT".to_string()),
            SqlValue::Text("rsduck MySQL protocol compatibility engine".to_string()),
            SqlValue::Text("YES".to_string()),
            SqlValue::Text("NO".to_string()),
            SqlValue::Text("YES".to_string()),
        ]],
    }
}

fn show_variables_like_pattern(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let lower = trimmed.to_ascii_lowercase();
    let prefix = "show variables like";
    if !lower.starts_with(prefix) {
        return None;
    }
    let rest = trimmed[prefix.len()..].trim();
    parse_single_quoted_literal(rest)
}

fn parse_single_quoted_literal(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut out = String::new();
    let mut idx = 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' if bytes.get(idx + 1) == Some(&b'\'') => {
                out.push('\'');
                idx += 2;
            }
            b'\'' => {
                if input[idx + 1..].trim().is_empty() {
                    return Some(out);
                }
                return None;
            }
            _ => {
                let next = next_sql_char_index(input, idx);
                out.push_str(&input[idx..next]);
                idx = next;
            }
        }
    }
    None
}

fn next_sql_char_index(sql: &str, idx: usize) -> usize {
    idx + sql[idx..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn system_variables_result(pattern: Option<&str>) -> SqlTypedResult {
    let rows = MYSQL_SYSTEM_VARIABLES
        .iter()
        .filter(|(name, _)| pattern.map_or(true, |pattern| mysql_like_matches(pattern, name)))
        .map(|(name, value)| {
            vec![
                SqlValue::Text((*name).to_string()),
                SqlValue::Text((*value).to_string()),
            ]
        })
        .collect();
    SqlTypedResult::Query {
        columns: vec![
            SqlColumn {
                name: "Variable_name".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "Value".to_string(),
                data_type: SqlType::Text,
            },
        ],
        rows,
    }
}

const MYSQL_SYSTEM_VARIABLES: &[(&str, &str)] = &[
    ("character_set_client", "utf8mb4"),
    ("character_set_connection", "utf8mb4"),
    ("character_set_database", "utf8mb4"),
    ("character_set_results", "utf8mb4"),
    ("character_set_server", "utf8mb4"),
    ("collation_connection", "utf8mb4_general_ci"),
    ("collation_database", "utf8mb4_general_ci"),
    ("collation_server", "utf8mb4_general_ci"),
    ("lower_case_file_system", "ON"),
    ("lower_case_table_names", "1"),
    ("max_allowed_packet", "16777216"),
    ("sql_mode", ""),
    ("system_time_zone", "UTC"),
    ("time_zone", "SYSTEM"),
    ("transaction_isolation", "READ-COMMITTED"),
    ("version", "8.0.34-rsduck"),
    ("version_comment", "rsduck MySQL protocol"),
];

fn mysql_like_matches(pattern: &str, value: &str) -> bool {
    fn inner(pattern: &[u8], value: &[u8]) -> bool {
        if pattern.is_empty() {
            return value.is_empty();
        }
        match pattern[0] {
            b'%' => {
                inner(&pattern[1..], value) || (!value.is_empty() && inner(pattern, &value[1..]))
            }
            b'_' => !value.is_empty() && inner(&pattern[1..], &value[1..]),
            b'\\' if pattern.len() > 1 => {
                !value.is_empty()
                    && pattern[1].eq_ignore_ascii_case(&value[0])
                    && inner(&pattern[2..], &value[1..])
            }
            byte => {
                !value.is_empty()
                    && byte.eq_ignore_ascii_case(&value[0])
                    && inner(&pattern[1..], &value[1..])
            }
        }
    }

    inner(pattern.as_bytes(), value.as_bytes())
}

fn dummy_params_for_describe(sql: &str, param_count: usize) -> Vec<SqlParam> {
    (1..=param_count)
        .map(|idx| {
            let type_name = cast_type_after_placeholder(sql, idx).unwrap_or_default();
            match type_name {
                "bool" | "boolean" => SqlParam::Bool(false),
                "tinyint" | "smallint" | "int2" | "int4" | "int8" | "integer" | "int"
                | "bigint" => SqlParam::Integer(0),
                "float" | "real" | "double" | "float4" | "float8" | "numeric" | "decimal" => {
                    SqlParam::Float(0.0)
                }
                "blob" | "bytea" => SqlParam::Bytes(Vec::new()),
                "date" => SqlParam::Text("1970-01-01".to_string()),
                "time" => SqlParam::Text("00:00:00".to_string()),
                "timestamp" | "datetime" | "timestamptz" => {
                    SqlParam::Text("1970-01-01 00:00:00".to_string())
                }
                "uuid" => SqlParam::Text("00000000-0000-0000-0000-000000000000".to_string()),
                _ => SqlParam::Text(String::new()),
            }
        })
        .collect()
}

fn cast_type_after_placeholder(sql: &str, param_number: usize) -> Option<&str> {
    let lower = sql.to_ascii_lowercase();
    let needle = format!("${param_number}");
    let pos = lower.find(&needle)?;
    let mut idx = pos + needle.len();
    idx = skip_ascii_space(&lower, idx);
    if !lower[idx..].starts_with("::") {
        return None;
    }
    idx = skip_ascii_space(&lower, idx + 2);
    let start = idx;
    while lower
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        idx += 1;
    }
    if start == idx {
        None
    } else {
        Some(&sql[start..idx])
    }
}

fn skip_ascii_space(sql: &str, mut idx: usize) -> usize {
    while sql
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        idx += 1;
    }
    idx
}

fn single_text_row(column: &str, value: &str) -> SqlTypedResult {
    SqlTypedResult::Query {
        columns: vec![SqlColumn {
            name: column.to_string(),
            data_type: SqlType::Text,
        }],
        rows: vec![vec![SqlValue::Text(value.to_string())]],
    }
}

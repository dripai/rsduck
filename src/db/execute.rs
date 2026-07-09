#[cfg(test)]
fn execute_sql_blocking(
    conn: &Connection,
    username: &str,
    sql: &str,
    route: SqlRoute,
    command: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    execute_typed_sql_blocking(conn, username, sql, route, command, max_result_rows)
        .map(SqlResult::from)
}

fn execute_typed_sql_blocking(
    conn: &Connection,
    username: &str,
    sql: &str,
    route: SqlRoute,
    command: &str,
    max_result_rows: usize,
) -> Result<SqlTypedResult, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if crate::catalog::looks_like_privilege_function(sql_trimmed) {
        let (column, allowed) =
            crate::catalog::evaluate_privilege_function(conn, username, sql_trimmed)?;
        return Ok(SqlTypedResult::Query {
            columns: vec![SqlColumn::new(column, PG_TYPE_BOOL)],
            rows: vec![vec![Some(if allowed { "t" } else { "f" }.to_string())]],
        });
    }

    if let Some(result) = crate::pg_compat::compat_result(sql_trimmed, username) {
        return Ok(typed_result_from_sql_result(result));
    }
    if let Some(rewritten_sql) = crate::pg_compat::rewrite_sql(sql_trimmed) {
        crate::catalog::authorize_catalog_projection(conn, username)?;
        return query_typed_sql_blocking(conn, &rewritten_sql, max_result_rows);
    }
    if crate::catalog::is_reserved_diagnostic_read(sql_trimmed) {
        crate::catalog::authorize_reserved_diagnostic(conn, username, sql_trimmed)?;
        return query_typed_sql_blocking(conn, sql_trimmed, max_result_rows);
    }
    crate::catalog::guard_external_sql_as(username, sql_trimmed)?;
    crate::catalog::reject_unhandled_catalog_projection(sql_trimmed)?;
    crate::catalog::authorize_sql(conn, username, sql_trimmed)?;

    match route {
        SqlRoute::Read => query_typed_sql_blocking(conn, sql_trimmed, max_result_rows),
        SqlRoute::Write => {
            let affected_rows = match crate::catalog::execute_catalog_aware_write_as(
                conn,
                username,
                sql_trimmed,
            )? {
                Some(affected_rows) => affected_rows,
                None => conn.execute(sql_trimmed, []).map_err(|e| e.to_string())?,
            };
            Ok(SqlTypedResult::Execute {
                command: command.to_string(),
                affected_rows,
            })
        }
    }
}

pub fn sql_placeholder_count(sql: &str) -> Result<usize, String> {
    scan_sql_params(sql, None).map(|(_, count)| count)
}

fn bind_sql_params(sql: &str, params: &[SqlParam]) -> Result<String, String> {
    scan_sql_params(sql, Some(params)).map(|(sql, _)| sql)
}

fn scan_sql_params(
    sql: &str,
    params: Option<&[SqlParam]>,
) -> Result<(String, usize), String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut max_param = 0;
    let mut state = SqlScanState::Normal;

    while idx < bytes.len() {
        match state {
            SqlScanState::Normal => {
                if bytes[idx] == b'\'' {
                    let next = idx + 1;
                    out.push_str(&sql[idx..next]);
                    idx = next;
                    state = SqlScanState::SingleQuote;
                } else if bytes[idx] == b'"' {
                    let next = idx + 1;
                    out.push_str(&sql[idx..next]);
                    idx = next;
                    state = SqlScanState::DoubleQuote;
                } else if bytes[idx] == b'-' && bytes.get(idx + 1) == Some(&b'-') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = SqlScanState::LineComment;
                } else if bytes[idx] == b'/' && bytes.get(idx + 1) == Some(&b'*') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = SqlScanState::BlockComment;
                } else if bytes[idx] == b'$'
                    && bytes
                        .get(idx + 1)
                        .is_some_and(|byte| byte.is_ascii_digit())
                {
                    let start = idx + 1;
                    let mut end = start;
                    while bytes.get(end).is_some_and(|byte| byte.is_ascii_digit()) {
                        end += 1;
                    }
                    let param_number = sql[start..end]
                        .parse::<usize>()
                        .map_err(|_| format!("invalid SQL parameter: ${}", &sql[start..end]))?;
                    if param_number == 0 {
                        return Err("invalid SQL parameter: $0".into());
                    }
                    max_param = max_param.max(param_number);
                    if let Some(params) = params {
                        let param = params.get(param_number - 1).ok_or_else(|| {
                            format!("missing SQL parameter: ${param_number}")
                        })?;
                        out.push_str(&sql_param_literal(param)?);
                    } else {
                        out.push_str(&sql[idx..end]);
                    }
                    idx = end;
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            SqlScanState::SingleQuote => {
                if bytes[idx] == b'\'' {
                    if bytes.get(idx + 1) == Some(&b'\'') {
                        out.push_str(&sql[idx..idx + 2]);
                        idx += 2;
                    } else {
                        out.push_str(&sql[idx..idx + 1]);
                        idx += 1;
                        state = SqlScanState::Normal;
                    }
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            SqlScanState::DoubleQuote => {
                if bytes[idx] == b'"' {
                    if bytes.get(idx + 1) == Some(&b'"') {
                        out.push_str(&sql[idx..idx + 2]);
                        idx += 2;
                    } else {
                        out.push_str(&sql[idx..idx + 1]);
                        idx += 1;
                        state = SqlScanState::Normal;
                    }
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            SqlScanState::LineComment => {
                let next = next_char_index(sql, idx);
                out.push_str(&sql[idx..next]);
                if bytes[idx] == b'\n' {
                    state = SqlScanState::Normal;
                }
                idx = next;
            }
            SqlScanState::BlockComment => {
                if bytes[idx] == b'*' && bytes.get(idx + 1) == Some(&b'/') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = SqlScanState::Normal;
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
        }
    }

    if let Some(params) = params {
        if params.len() > max_param {
            return Err(format!(
                "too many SQL parameters: got {}, used {}",
                params.len(),
                max_param
            ));
        }
    }

    Ok((out, max_param))
}

#[derive(Debug, Clone, Copy)]
enum SqlScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
}

fn next_char_index(sql: &str, idx: usize) -> usize {
    idx + sql[idx..]
        .chars()
        .next()
        .map(char::len_utf8)
        .unwrap_or(1)
}

fn sql_param_literal(param: &SqlParam) -> Result<String, String> {
    match param {
        SqlParam::Null => Ok("NULL".to_string()),
        SqlParam::Text(value) => Ok(sql_string_literal(value)),
        SqlParam::Bool(value) => Ok(if *value { "true" } else { "false" }.to_string()),
        SqlParam::Integer(value) => Ok(value.to_string()),
        SqlParam::Float(value) => {
            if value.is_finite() {
                Ok(value.to_string())
            } else {
                Err(format!("non-finite SQL parameter is not supported: {value}"))
            }
        }
        SqlParam::Bytes(value) => Ok(format!("'\\x{}'", hex_encode(value))),
    }
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn query_typed_sql_blocking(
    conn: &Connection,
    sql: &str,
    max_result_rows: usize,
) -> Result<SqlTypedResult, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "query did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    let cols = statement_columns(stmt_ref, col_count);
    let mut data = Vec::new();

    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if data.len() >= max_result_rows {
            return Err(format!("result row limit exceeded: {max_result_rows}"));
        }
        let mut line = Vec::with_capacity(cols.len());
        for idx in 0..cols.len() {
            line.push(cell_to_pg_text(row, idx));
        }
        data.push(line);
    }

    Ok(SqlTypedResult::Query {
        columns: cols,
        rows: data,
    })
}

fn describe_sql_blocking(
    conn: &Connection,
    username: &str,
    sql: &str,
    route: SqlRoute,
) -> Result<Vec<SqlColumn>, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if crate::catalog::looks_like_privilege_function(sql_trimmed) {
        let (column, _) = crate::catalog::evaluate_privilege_function(conn, username, sql_trimmed)?;
        return Ok(vec![SqlColumn::new(column, PG_TYPE_BOOL)]);
    }

    if let Some(result) = crate::pg_compat::compat_result(sql_trimmed, username) {
        return Ok(match result {
            SqlResult::Query { columns, .. } => {
                columns.into_iter().map(SqlColumn::text).collect()
            }
            SqlResult::Execute { .. } => Vec::new(),
        });
    }
    if let Some(rewritten_sql) = crate::pg_compat::rewrite_sql(sql_trimmed) {
        crate::catalog::authorize_catalog_projection(conn, username)?;
        return describe_query_sql_blocking(conn, &rewritten_sql);
    }
    if crate::catalog::is_reserved_diagnostic_read(sql_trimmed) {
        crate::catalog::authorize_reserved_diagnostic(conn, username, sql_trimmed)?;
        return describe_query_sql_blocking(conn, sql_trimmed);
    }
    crate::catalog::guard_external_sql_as(username, sql_trimmed)?;
    crate::catalog::reject_unhandled_catalog_projection(sql_trimmed)?;
    crate::catalog::authorize_sql(conn, username, sql_trimmed)?;

    match route {
        SqlRoute::Read => describe_query_sql_blocking(conn, sql_trimmed),
        SqlRoute::Write => Ok(Vec::new()),
    }
}

fn describe_query_sql_blocking(conn: &Connection, sql: &str) -> Result<Vec<SqlColumn>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt.query([]).map_err(|e| e.to_string())?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "query did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    Ok(statement_columns(stmt_ref, col_count))
}

fn typed_result_from_sql_result(result: SqlResult) -> SqlTypedResult {
    match result {
        SqlResult::Query { columns, rows } => SqlTypedResult::Query {
            columns: columns.into_iter().map(SqlColumn::text).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(Some).collect())
                .collect(),
        },
        SqlResult::Execute {
            command,
            affected_rows,
        } => SqlTypedResult::Execute {
            command,
            affected_rows,
        },
    }
}

fn statement_columns(stmt: &duckdb::Statement<'_>, col_count: usize) -> Vec<SqlColumn> {
    (0..col_count)
        .map(|idx| {
            let name = stmt
                .column_name(idx)
                .map(|name| name.to_string())
                .unwrap_or_else(|_| format!("column_{idx}"));
            let logical_type = stmt.column_logical_type(idx);
            let type_id = logical_type
                .try_id()
                .unwrap_or(duckdb::core::LogicalTypeId::Unsupported);
            SqlColumn::new(name, pg_type_oid_for_duckdb_type(type_id))
        })
        .collect()
}

fn pg_type_oid_for_duckdb_type(type_id: duckdb::core::LogicalTypeId) -> u32 {
    use duckdb::core::LogicalTypeId;

    match type_id {
        LogicalTypeId::Boolean => PG_TYPE_BOOL,
        LogicalTypeId::Tinyint | LogicalTypeId::Smallint | LogicalTypeId::UTinyint => PG_TYPE_INT2,
        LogicalTypeId::Integer | LogicalTypeId::USmallint => PG_TYPE_INT4,
        LogicalTypeId::Bigint | LogicalTypeId::UInteger | LogicalTypeId::IntegerLiteral => {
            PG_TYPE_INT8
        }
        LogicalTypeId::Hugeint
        | LogicalTypeId::UHugeint
        | LogicalTypeId::UBigint
        | LogicalTypeId::Decimal
        | LogicalTypeId::Bignum => PG_TYPE_NUMERIC,
        LogicalTypeId::Float => PG_TYPE_FLOAT4,
        LogicalTypeId::Double => PG_TYPE_FLOAT8,
        LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampS
        | LogicalTypeId::TimestampMs
        | LogicalTypeId::TimestampNs => PG_TYPE_TIMESTAMP,
        LogicalTypeId::TimestampTZ => PG_TYPE_TIMESTAMPTZ,
        LogicalTypeId::Date => PG_TYPE_DATE,
        LogicalTypeId::Time | LogicalTypeId::TimeNs => PG_TYPE_TIME,
        LogicalTypeId::Uuid => PG_TYPE_UUID,
        LogicalTypeId::Blob => PG_TYPE_BYTEA,
        LogicalTypeId::Varchar
        | LogicalTypeId::StringLiteral
        | LogicalTypeId::Enum
        | LogicalTypeId::Interval
        | LogicalTypeId::List
        | LogicalTypeId::Struct
        | LogicalTypeId::Map
        | LogicalTypeId::Union
        | LogicalTypeId::Bit
        | LogicalTypeId::TimeTZ
        | LogicalTypeId::Array
        | LogicalTypeId::Any
        | LogicalTypeId::SqlNull
        | LogicalTypeId::Variant
        | LogicalTypeId::Invalid
        | LogicalTypeId::Unsupported => PG_TYPE_TEXT,
        _ => PG_TYPE_TEXT,
    }
}

fn cell_to_pg_text(row: &duckdb::Row<'_>, idx: usize) -> Option<String> {
    row.get_ref(idx)
        .ok()
        .and_then(value_ref_to_pg_text)
}

fn value_ref_to_pg_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Boolean(v) => Some(if v { "t" } else { "f" }.to_string()),
        ValueRef::TinyInt(v) => Some(v.to_string()),
        ValueRef::SmallInt(v) => Some(v.to_string()),
        ValueRef::Int(v) => Some(v.to_string()),
        ValueRef::BigInt(v) => Some(v.to_string()),
        ValueRef::HugeInt(v) => Some(v.to_string()),
        ValueRef::UTinyInt(v) => Some(v.to_string()),
        ValueRef::USmallInt(v) => Some(v.to_string()),
        ValueRef::UInt(v) => Some(v.to_string()),
        ValueRef::UBigInt(v) => Some(v.to_string()),
        ValueRef::Float(v) => Some(v.to_string()),
        ValueRef::Double(v) => Some(v.to_string()),
        ValueRef::Decimal(v) => Some(v.to_string()),
        ValueRef::Timestamp(unit, value) => Some(format_timestamp(unit, value)),
        ValueRef::Text(v) => Some(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => Some(format!("\\x{}", hex_encode(v))),
        ValueRef::Date32(v) => Some(format_date32(v)),
        ValueRef::Time64(unit, value) => Some(format_time64(unit, value)),
        ValueRef::Interval {
            months,
            days,
            nanos,
        } => Some(format!("{months} months {days} days {nanos} ns")),
        other => Some(format!("{other:?}")),
    }
}

fn format_date32(days: i32) -> String {
    let Some(epoch) = chrono::NaiveDate::from_ymd_opt(1970, 1, 1) else {
        return days.to_string();
    };
    epoch
        .checked_add_signed(chrono::Duration::days(days as i64))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

fn format_timestamp(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let micros = unit.to_micros(value);
    let secs = micros.div_euclid(1_000_000);
    let nanos = micros.rem_euclid(1_000_000) as u32 * 1_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
        .map(|datetime| {
            datetime
                .naive_utc()
                .format("%Y-%m-%d %H:%M:%S%.6f")
                .to_string()
        })
        .unwrap_or_else(|| format!("{value} {unit:?}"))
}

fn format_time64(unit: duckdb::types::TimeUnit, value: i64) -> String {
    let micros_per_day = 86_400_000_000_i64;
    let micros = unit.to_micros(value).rem_euclid(micros_per_day);
    let hours = micros / 3_600_000_000;
    let minutes = (micros / 60_000_000) % 60;
    let seconds = (micros / 1_000_000) % 60;
    let subsecond = micros % 1_000_000;
    if subsecond == 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{subsecond:06}")
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

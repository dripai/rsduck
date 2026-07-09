use super::*;

#[cfg(test)]
pub(super) fn execute_sql_blocking(
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

pub(super) fn execute_typed_sql_blocking(
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
            columns: vec![SqlColumn::new(column, SqlType::Bool)],
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

pub(super) fn query_typed_sql_blocking(
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

pub(super) fn describe_sql_blocking(
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
        return Ok(vec![SqlColumn::new(column, SqlType::Bool)]);
    }

    if let Some(result) = crate::pg_compat::compat_result(sql_trimmed, username) {
        return Ok(match result {
            SqlResult::Query { columns, .. } => columns.into_iter().map(SqlColumn::text).collect(),
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

pub(super) fn describe_query_sql_blocking(
    conn: &Connection,
    sql: &str,
) -> Result<Vec<SqlColumn>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt.query([]).map_err(|e| e.to_string())?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "query did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    Ok(statement_columns(stmt_ref, col_count))
}

pub(super) fn typed_result_from_sql_result(result: SqlResult) -> SqlTypedResult {
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

pub(super) fn statement_columns(stmt: &duckdb::Statement<'_>, col_count: usize) -> Vec<SqlColumn> {
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
            SqlColumn::new(name, sql_type_for_duckdb_type(type_id))
        })
        .collect()
}

pub(super) fn sql_type_for_duckdb_type(type_id: duckdb::core::LogicalTypeId) -> SqlType {
    use duckdb::core::LogicalTypeId;

    match type_id {
        LogicalTypeId::Boolean => SqlType::Bool,
        LogicalTypeId::Tinyint | LogicalTypeId::Smallint | LogicalTypeId::UTinyint => SqlType::Int2,
        LogicalTypeId::Integer | LogicalTypeId::USmallint => SqlType::Int4,
        LogicalTypeId::Bigint | LogicalTypeId::UInteger | LogicalTypeId::IntegerLiteral => {
            SqlType::Int8
        }
        LogicalTypeId::Hugeint
        | LogicalTypeId::UHugeint
        | LogicalTypeId::UBigint
        | LogicalTypeId::Decimal
        | LogicalTypeId::Bignum => SqlType::Numeric,
        LogicalTypeId::Float => SqlType::Float4,
        LogicalTypeId::Double => SqlType::Float8,
        LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampS
        | LogicalTypeId::TimestampMs
        | LogicalTypeId::TimestampNs => SqlType::Timestamp,
        LogicalTypeId::TimestampTZ => SqlType::TimestampTz,
        LogicalTypeId::Date => SqlType::Date,
        LogicalTypeId::Time | LogicalTypeId::TimeNs => SqlType::Time,
        LogicalTypeId::Uuid => SqlType::Uuid,
        LogicalTypeId::Blob => SqlType::Bytea,
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
        | LogicalTypeId::Unsupported => SqlType::Text,
        _ => SqlType::Text,
    }
}

pub(super) fn cell_to_pg_text(row: &duckdb::Row<'_>, idx: usize) -> Option<String> {
    row.get_ref(idx).ok().and_then(value_ref_to_pg_text)
}

pub(super) fn value_ref_to_pg_text(value: ValueRef<'_>) -> Option<String> {
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

pub(super) fn format_date32(days: i32) -> String {
    let Some(epoch) = chrono::NaiveDate::from_ymd_opt(1970, 1, 1) else {
        return days.to_string();
    };
    epoch
        .checked_add_signed(chrono::Duration::days(days as i64))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

pub(super) fn format_timestamp(unit: duckdb::types::TimeUnit, value: i64) -> String {
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

pub(super) fn format_time64(unit: duckdb::types::TimeUnit, value: i64) -> String {
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

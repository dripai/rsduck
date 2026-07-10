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

    if let Some(result) = crate::mysql_compat::compat_result(sql_trimmed) {
        return Ok(result);
    }
    if let Some(result) = crate::catalog::show_partitions_result(conn, username, sql_trimmed)? {
        return Ok(result);
    }
    if let Some(rewritten_sql) = crate::mysql_compat::rewrite_sql(sql_trimmed, "main", username) {
        if crate::mysql_compat::is_mysql_system_projection(sql_trimmed) {
            crate::catalog::authorize_user_metadata(conn, username)?;
        } else {
            crate::catalog::authorize_catalog_projection(conn, username)?;
        }
        crate::mysql_compat::validate_metadata_projection(conn, username)?;
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
            line.push(cell_to_sql_value(row, idx, cols[idx].data_type));
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

    if let Some(result) = crate::mysql_compat::compat_result(sql_trimmed) {
        return Ok(match result {
            SqlTypedResult::Query { columns, .. } => columns,
            SqlTypedResult::Execute { .. } => Vec::new(),
        });
    }
    if let Some(result) = crate::catalog::show_partitions_result(conn, username, sql_trimmed)? {
        return Ok(match result {
            SqlTypedResult::Query { columns, .. } => columns,
            SqlTypedResult::Execute { .. } => Vec::new(),
        });
    }
    if let Some(rewritten_sql) = crate::mysql_compat::rewrite_sql(sql_trimmed, "main", username) {
        if crate::mysql_compat::is_mysql_system_projection(sql_trimmed) {
            crate::catalog::authorize_user_metadata(conn, username)?;
        } else {
            crate::catalog::authorize_catalog_projection(conn, username)?;
        }
        crate::mysql_compat::validate_metadata_projection(conn, username)?;
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
        | LogicalTypeId::Bit
        | LogicalTypeId::TimeTZ
        | LogicalTypeId::Any
        | LogicalTypeId::SqlNull
        | LogicalTypeId::Invalid
        | LogicalTypeId::Unsupported => SqlType::Text,
        LogicalTypeId::List
        | LogicalTypeId::Struct
        | LogicalTypeId::Map
        | LogicalTypeId::Union
        | LogicalTypeId::Array
        | LogicalTypeId::Variant => SqlType::Json,
        _ => SqlType::Text,
    }
}

pub(super) fn cell_to_sql_value(row: &duckdb::Row<'_>, idx: usize, data_type: SqlType) -> SqlValue {
    row.get_ref(idx)
        .map(|value| value_ref_to_sql_value(value, data_type))
        .unwrap_or(SqlValue::Null)
}

pub(super) fn value_ref_to_sql_value(value: ValueRef<'_>, data_type: SqlType) -> SqlValue {
    match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Boolean(v) => SqlValue::Bool(v),
        ValueRef::TinyInt(v) => SqlValue::Int16(v as i16),
        ValueRef::SmallInt(v) => SqlValue::Int16(v),
        ValueRef::Int(v) => SqlValue::Int32(v),
        ValueRef::BigInt(v) => SqlValue::Int64(v),
        ValueRef::HugeInt(v) => decimal_from_i128(v)
            .map(SqlValue::Decimal)
            .unwrap_or_else(|| SqlValue::NumericText(v.to_string())),
        ValueRef::UTinyInt(v) => SqlValue::Int16(v as i16),
        ValueRef::USmallInt(v) => SqlValue::Int32(v as i32),
        ValueRef::UInt(v) => SqlValue::Int64(v as i64),
        ValueRef::UBigInt(v) => SqlValue::Decimal(rust_decimal::Decimal::from(v)),
        ValueRef::Float(v) => SqlValue::Float32(v),
        ValueRef::Double(v) => SqlValue::Float64(v),
        ValueRef::Decimal(v) => SqlValue::Decimal(v),
        ValueRef::Timestamp(unit, value) => timestamp_value(unit, value, data_type),
        ValueRef::Text(v) => {
            let text = String::from_utf8_lossy(v).into_owned();
            if data_type == SqlType::Uuid {
                uuid::Uuid::parse_str(&text)
                    .map(SqlValue::Uuid)
                    .unwrap_or(SqlValue::Text(text))
            } else {
                SqlValue::Text(text)
            }
        }
        ValueRef::Blob(v) => SqlValue::Bytes(v.to_vec()),
        ValueRef::Date32(v) => date32_to_date(v)
            .map(SqlValue::Date)
            .unwrap_or_else(|| SqlValue::Text(v.to_string())),
        ValueRef::Time64(unit, value) => time64_to_time(unit, value)
            .map(SqlValue::Time)
            .unwrap_or_else(|| SqlValue::Text(format_time64(unit, value))),
        ValueRef::Interval {
            months,
            days,
            nanos,
        } => SqlValue::Interval {
            months,
            days,
            nanos,
        },
        ValueRef::Enum(enum_type, idx) => enum_value_to_text(enum_type, idx)
            .map(SqlValue::Text)
            .unwrap_or_else(|| SqlValue::Json(duckdb_value_to_json(value.to_owned()))),
        ValueRef::List(..)
        | ValueRef::Struct(..)
        | ValueRef::Array(..)
        | ValueRef::Map(..)
        | ValueRef::Union(..) => SqlValue::Json(duckdb_value_to_json(value.to_owned())),
    }
}

fn decimal_from_i128(value: i128) -> Option<rust_decimal::Decimal> {
    rust_decimal::Decimal::try_from_i128_with_scale(value, 0).ok()
}

fn enum_value_to_text(enum_type: duckdb::types::EnumType<'_>, idx: usize) -> Option<String> {
    ValueRef::Enum(enum_type, idx)
        .as_str()
        .ok()
        .map(str::to_string)
}

fn timestamp_value(unit: duckdb::types::TimeUnit, value: i64, data_type: SqlType) -> SqlValue {
    let Some(timestamp) = timestamp_to_utc(unit, value) else {
        return SqlValue::Text(format_timestamp(unit, value));
    };
    if data_type == SqlType::TimestampTz {
        SqlValue::TimestampTz(timestamp)
    } else {
        SqlValue::Timestamp(timestamp.naive_utc())
    }
}

fn timestamp_to_utc(
    unit: duckdb::types::TimeUnit,
    value: i64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let micros = unit.to_micros(value);
    let secs = micros.div_euclid(1_000_000);
    let nanos = micros.rem_euclid(1_000_000) as u32 * 1_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
}

pub(super) fn format_date32(days: i32) -> String {
    date32_to_date(days)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

fn date32_to_date(days: i32) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?
        .checked_add_signed(chrono::Duration::days(days as i64))
}

pub(super) fn format_timestamp(unit: duckdb::types::TimeUnit, value: i64) -> String {
    timestamp_to_utc(unit, value)
        .map(|datetime| {
            datetime
                .naive_utc()
                .format("%Y-%m-%d %H:%M:%S%.6f")
                .to_string()
        })
        .unwrap_or_else(|| format!("{value} {unit:?}"))
}

pub(super) fn format_time64(unit: duckdb::types::TimeUnit, value: i64) -> String {
    time64_to_time(unit, value)
        .map(|time| time.format("%H:%M:%S%.6f").to_string())
        .unwrap_or_else(|| format!("{value} {unit:?}"))
}

fn time64_to_time(unit: duckdb::types::TimeUnit, value: i64) -> Option<chrono::NaiveTime> {
    let micros_per_day = 86_400_000_000_i64;
    let micros = unit.to_micros(value).rem_euclid(micros_per_day);
    let seconds = micros / 1_000_000;
    let nanos = (micros % 1_000_000) as u32 * 1_000;
    chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds as u32, nanos)
}

fn duckdb_value_to_json(value: duckdb::types::Value) -> serde_json::Value {
    match value {
        duckdb::types::Value::Null => serde_json::Value::Null,
        duckdb::types::Value::Boolean(v) => serde_json::Value::Bool(v),
        duckdb::types::Value::TinyInt(v) => number_json(v),
        duckdb::types::Value::SmallInt(v) => number_json(v),
        duckdb::types::Value::Int(v) => number_json(v),
        duckdb::types::Value::BigInt(v) => number_json(v),
        duckdb::types::Value::HugeInt(v) => serde_json::Value::String(v.to_string()),
        duckdb::types::Value::UTinyInt(v) => number_json(v),
        duckdb::types::Value::USmallInt(v) => number_json(v),
        duckdb::types::Value::UInt(v) => number_json(v),
        duckdb::types::Value::UBigInt(v) => number_json(v),
        duckdb::types::Value::Float(v) => float_json(v as f64),
        duckdb::types::Value::Double(v) => float_json(v),
        duckdb::types::Value::Decimal(v) => serde_json::Value::String(v.to_string()),
        duckdb::types::Value::Timestamp(unit, value) => {
            serde_json::Value::String(format_timestamp(unit, value))
        }
        duckdb::types::Value::Text(v) => serde_json::Value::String(v),
        duckdb::types::Value::Blob(v) => {
            serde_json::Value::String(format!("\\x{}", hex_encode(&v)))
        }
        duckdb::types::Value::Date32(v) => serde_json::Value::String(format_date32(v)),
        duckdb::types::Value::Time64(unit, value) => {
            serde_json::Value::String(format_time64(unit, value))
        }
        duckdb::types::Value::Interval {
            months,
            days,
            nanos,
        } => serde_json::json!({
            "months": months,
            "days": days,
            "nanos": nanos
        }),
        duckdb::types::Value::List(values) | duckdb::types::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(duckdb_value_to_json).collect())
        }
        duckdb::types::Value::Enum(value) => serde_json::Value::String(value),
        duckdb::types::Value::Struct(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), duckdb_value_to_json(value.clone())))
                .collect(),
        ),
        duckdb::types::Value::Map(entries) => serde_json::Value::Array(
            entries
                .iter()
                .map(|(key, value)| {
                    serde_json::json!({
                        "key": duckdb_value_to_json(key.clone()),
                        "value": duckdb_value_to_json(value.clone())
                    })
                })
                .collect(),
        ),
        duckdb::types::Value::Union(value) => duckdb_value_to_json(*value),
    }
}

fn number_json(value: impl Into<serde_json::Number>) -> serde_json::Value {
    serde_json::Value::Number(value.into())
}

fn float_json(value: f64) -> serde_json::Value {
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(value.to_string()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

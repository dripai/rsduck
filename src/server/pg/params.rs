use pgwire::api::portal::Portal;
use pgwire::api::results::FieldFormat;
use pgwire::api::stmt::StoredStatement;
use pgwire::api::Type;

use crate::db::SqlParam;

pub(super) fn sql_params_from_portal(portal: &Portal<String>) -> Result<Vec<SqlParam>, String> {
    let mut params = Vec::with_capacity(portal.parameters.len());
    for (idx, param) in portal.parameters.iter().enumerate() {
        let data_type = portal
            .statement
            .parameter_types
            .get(idx)
            .cloned()
            .unwrap_or(Type::UNKNOWN);
        let format = portal.parameter_format.format_for(idx);
        params.push(sql_param_from_pg_value(
            param.as_ref().map(|bytes| bytes.as_ref()),
            &data_type,
            format,
        )?);
    }
    Ok(params)
}

fn sql_param_from_pg_value(
    value: Option<&[u8]>,
    data_type: &Type,
    format: FieldFormat,
) -> Result<SqlParam, String> {
    let Some(value) = value else {
        return Ok(SqlParam::Null);
    };

    if format == FieldFormat::Text {
        let text = std::str::from_utf8(value)
            .map_err(|e| format!("invalid UTF-8 text SQL parameter: {e}"))?;
        return sql_param_from_text(text, data_type);
    }

    match *data_type {
        Type::BOOL => {
            let byte = single_byte(value, data_type)?;
            Ok(SqlParam::Bool(byte != 0))
        }
        Type::INT2 => Ok(SqlParam::Integer(
            i16::from_be_bytes(fixed_bytes(value, data_type)?) as i64,
        )),
        Type::INT4 => Ok(SqlParam::Integer(
            i32::from_be_bytes(fixed_bytes(value, data_type)?) as i64,
        )),
        Type::INT8 => Ok(SqlParam::Integer(i64::from_be_bytes(fixed_bytes(
            value, data_type,
        )?))),
        Type::FLOAT4 => Ok(SqlParam::Float(
            f32::from_bits(u32::from_be_bytes(fixed_bytes(value, data_type)?)) as f64,
        )),
        Type::FLOAT8 => Ok(SqlParam::Float(f64::from_bits(u64::from_be_bytes(
            fixed_bytes(value, data_type)?,
        )))),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            let text = std::str::from_utf8(value)
                .map_err(|e| format!("invalid UTF-8 binary SQL parameter: {e}"))?;
            Ok(SqlParam::Text(text.to_string()))
        }
        Type::DATE => {
            let days = i32::from_be_bytes(fixed_bytes(value, data_type)?);
            let date = pg_epoch_date()
                .checked_add_signed(chrono::Duration::days(days as i64))
                .ok_or_else(|| format!("date SQL parameter is out of range: {days}"))?;
            Ok(SqlParam::Text(date.format("%Y-%m-%d").to_string()))
        }
        Type::TIME => {
            let micros = i64::from_be_bytes(fixed_bytes(value, data_type)?);
            Ok(SqlParam::Text(format_time_micros(micros)?))
        }
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(fixed_bytes(value, data_type)?);
            let timestamp = pg_epoch_datetime()
                .checked_add_signed(chrono::Duration::microseconds(micros))
                .ok_or_else(|| format!("timestamp SQL parameter is out of range: {micros}"))?;
            Ok(SqlParam::Text(
                timestamp.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
            ))
        }
        Type::BYTEA => Ok(SqlParam::Bytes(value.to_vec())),
        Type::NUMERIC => {
            let text = std::str::from_utf8(value)
                .map_err(|_| "binary numeric SQL parameters are not supported".to_string())?;
            sql_param_from_text(text, data_type)
        }
        _ => Err(format!(
            "unsupported PG binary SQL parameter type: {}",
            data_type.name()
        )),
    }
}

fn sql_param_from_text(value: &str, data_type: &Type) -> Result<SqlParam, String> {
    match *data_type {
        Type::BOOL => parse_pg_bool_text(value).map(SqlParam::Bool),
        Type::INT2 | Type::INT4 | Type::INT8 => value
            .parse::<i64>()
            .map(SqlParam::Integer)
            .map_err(|e| format!("invalid integer SQL parameter '{value}': {e}")),
        Type::FLOAT4 | Type::FLOAT8 => value
            .parse::<f64>()
            .map(SqlParam::Float)
            .map_err(|e| format!("invalid float SQL parameter '{value}': {e}")),
        Type::BYTEA => parse_pg_bytea_text(value).map(SqlParam::Bytes),
        _ => Ok(SqlParam::Text(value.to_string())),
    }
}

pub(super) fn infer_statement_parameter_types(statement: &StoredStatement<String>) -> Vec<Type> {
    infer_parameter_types(&statement.statement, &statement.parameter_types)
}

pub(super) fn infer_parameter_types(sql: &str, provided_types: &[Type]) -> Vec<Type> {
    let placeholder_count = crate::db::sql_placeholder_count(sql).unwrap_or_default();
    let count = placeholder_count.max(provided_types.len());
    (0..count)
        .map(|idx| {
            let provided = provided_types.get(idx).cloned().unwrap_or(Type::UNKNOWN);
            if provided != Type::UNKNOWN {
                provided
            } else {
                infer_cast_type_for_placeholder(sql, idx + 1).unwrap_or(Type::TEXT)
            }
        })
        .collect()
}

pub(super) fn dummy_sql_param_for_type(data_type: &Type) -> SqlParam {
    match *data_type {
        Type::BOOL => SqlParam::Bool(false),
        Type::INT2 | Type::INT4 | Type::INT8 => SqlParam::Integer(0),
        Type::FLOAT4 | Type::FLOAT8 => SqlParam::Float(0.0),
        Type::BYTEA => SqlParam::Bytes(Vec::new()),
        _ => SqlParam::Text(String::new()),
    }
}

fn infer_cast_type_for_placeholder(sql: &str, param_number: usize) -> Option<Type> {
    let lower = sql.to_ascii_lowercase();
    let needle = format!("${param_number}");
    for (pos, _) in lower.match_indices(&needle) {
        let after_placeholder = pos + needle.len();
        if lower
            .as_bytes()
            .get(after_placeholder)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Some(type_name) = cast_type_after_placeholder(&lower, after_placeholder) else {
            continue;
        };
        if let Some(data_type) = pg_type_by_name(type_name) {
            return Some(data_type);
        }
    }
    None
}

fn cast_type_after_placeholder(sql: &str, mut idx: usize) -> Option<&str> {
    idx = skip_ascii_space(sql, idx);
    if !sql[idx..].starts_with("::") {
        return None;
    }
    idx = skip_ascii_space(sql, idx + 2);
    let start = idx;
    while sql
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

fn pg_type_by_name(name: &str) -> Option<Type> {
    match name {
        "bool" | "boolean" => Some(Type::BOOL),
        "int2" | "smallint" => Some(Type::INT2),
        "int4" | "int" | "integer" => Some(Type::INT4),
        "int8" | "bigint" => Some(Type::INT8),
        "float4" | "real" => Some(Type::FLOAT4),
        "float8" | "double" => Some(Type::FLOAT8),
        "text" => Some(Type::TEXT),
        "varchar" => Some(Type::VARCHAR),
        "bpchar" | "char" => Some(Type::BPCHAR),
        "name" => Some(Type::NAME),
        "date" => Some(Type::DATE),
        "time" => Some(Type::TIME),
        "timestamp" => Some(Type::TIMESTAMP),
        "timestamptz" => Some(Type::TIMESTAMPTZ),
        "numeric" | "decimal" => Some(Type::NUMERIC),
        "bytea" | "blob" => Some(Type::BYTEA),
        _ => None,
    }
}

pub(super) fn parse_pg_bool_text(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "1" => Ok(true),
        "f" | "false" | "0" => Ok(false),
        _ => Err(format!("invalid boolean value: {value}")),
    }
}

pub(super) fn parse_pg_bytea_text(value: &str) -> Result<Vec<u8>, String> {
    let hex = value.strip_prefix("\\x").unwrap_or(value);
    if hex.len() % 2 != 0 {
        return Err(format!("invalid bytea hex length: {}", hex.len()));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for idx in (0..raw.len()).step_by(2) {
        let hi = hex_digit(raw[idx])?;
        let lo = hex_digit(raw[idx + 1])?;
        bytes.push((hi << 4) | lo);
    }
    Ok(bytes)
}

fn hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", value as char)),
    }
}

fn single_byte(value: &[u8], data_type: &Type) -> Result<u8, String> {
    if value.len() != 1 {
        return Err(format!(
            "invalid binary {} parameter length: expected 1, got {}",
            data_type.name(),
            value.len()
        ));
    }
    Ok(value[0])
}

fn fixed_bytes<const N: usize>(value: &[u8], data_type: &Type) -> Result<[u8; N], String> {
    value.try_into().map_err(|_| {
        format!(
            "invalid binary {} parameter length: expected {}, got {}",
            data_type.name(),
            N,
            value.len()
        )
    })
}

fn pg_epoch_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("valid PG epoch date")
}

fn pg_epoch_datetime() -> chrono::NaiveDateTime {
    pg_epoch_date()
        .and_hms_opt(0, 0, 0)
        .expect("valid PG epoch timestamp")
}

fn format_time_micros(micros: i64) -> Result<String, String> {
    let micros_per_day = 86_400_000_000_i64;
    if !(0..micros_per_day).contains(&micros) {
        return Err(format!("time SQL parameter is out of range: {micros}"));
    }
    let seconds = micros / 1_000_000;
    let nanos = (micros % 1_000_000) as u32 * 1_000;
    chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds as u32, nanos)
        .map(|time| time.format("%H:%M:%S%.6f").to_string())
        .ok_or_else(|| format!("time SQL parameter is out of range: {micros}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_explicit_cast_parameter_types() {
        assert_eq!(
            infer_cast_type_for_placeholder("select $1::varchar as ok", 1),
            Some(Type::VARCHAR)
        );
        assert_eq!(
            infer_cast_type_for_placeholder("select $2 :: integer as id", 2),
            Some(Type::INT4)
        );
        assert_eq!(infer_cast_type_for_placeholder("select '$1'", 1), None);
    }

    #[test]
    fn decodes_binary_pg_parameters_for_sql_binding() {
        assert_eq!(
            sql_param_from_pg_value(Some(&1_i32.to_be_bytes()), &Type::INT4, FieldFormat::Binary)
                .unwrap(),
            SqlParam::Integer(1)
        );
        assert_eq!(
            sql_param_from_pg_value(Some(b"ready"), &Type::TEXT, FieldFormat::Binary).unwrap(),
            SqlParam::Text("ready".to_string())
        );
    }
}

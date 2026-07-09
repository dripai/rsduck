use crate::db::SqlParam;

use super::codec::{get_lenenc_bytes, get_u16_le, get_u32_le, get_u64_le, take};
use super::types::{
    MYSQL_TYPE_BLOB, MYSQL_TYPE_DATE, MYSQL_TYPE_DATETIME, MYSQL_TYPE_DOUBLE, MYSQL_TYPE_FLOAT,
    MYSQL_TYPE_JSON, MYSQL_TYPE_LONG, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_NEWDECIMAL, MYSQL_TYPE_SHORT,
    MYSQL_TYPE_STRING, MYSQL_TYPE_TIME, MYSQL_TYPE_TIMESTAMP, MYSQL_TYPE_TINY,
    MYSQL_TYPE_VAR_STRING,
};

pub(super) fn rewrite_mysql_placeholders(sql: &str) -> Result<(String, usize), String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut count = 0;
    let mut state = ScanState::Normal;

    while idx < bytes.len() {
        match state {
            ScanState::Normal => {
                if bytes[idx] == b'\'' {
                    let next = idx + 1;
                    out.push_str(&sql[idx..next]);
                    idx = next;
                    state = ScanState::SingleQuote;
                } else if bytes[idx] == b'"' {
                    let next = idx + 1;
                    out.push_str(&sql[idx..next]);
                    idx = next;
                    state = ScanState::DoubleQuote;
                } else if bytes[idx] == b'-' && bytes.get(idx + 1) == Some(&b'-') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = ScanState::LineComment;
                } else if bytes[idx] == b'/' && bytes.get(idx + 1) == Some(&b'*') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = ScanState::BlockComment;
                } else if bytes[idx] == b'?' {
                    count += 1;
                    out.push('$');
                    out.push_str(&count.to_string());
                    idx += 1;
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            ScanState::SingleQuote => {
                if bytes[idx] == b'\'' {
                    if bytes.get(idx + 1) == Some(&b'\'') {
                        out.push_str(&sql[idx..idx + 2]);
                        idx += 2;
                    } else {
                        out.push_str(&sql[idx..idx + 1]);
                        idx += 1;
                        state = ScanState::Normal;
                    }
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            ScanState::DoubleQuote => {
                if bytes[idx] == b'"' {
                    if bytes.get(idx + 1) == Some(&b'"') {
                        out.push_str(&sql[idx..idx + 2]);
                        idx += 2;
                    } else {
                        out.push_str(&sql[idx..idx + 1]);
                        idx += 1;
                        state = ScanState::Normal;
                    }
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
            ScanState::LineComment => {
                let next = next_char_index(sql, idx);
                out.push_str(&sql[idx..next]);
                if bytes[idx] == b'\n' {
                    state = ScanState::Normal;
                }
                idx = next;
            }
            ScanState::BlockComment => {
                if bytes[idx] == b'*' && bytes.get(idx + 1) == Some(&b'/') {
                    out.push_str(&sql[idx..idx + 2]);
                    idx += 2;
                    state = ScanState::Normal;
                } else {
                    let next = next_char_index(sql, idx);
                    out.push_str(&sql[idx..next]);
                    idx = next;
                }
            }
        }
    }

    Ok((out, count))
}

pub(super) fn parse_execute_params(
    payload: &[u8],
    idx: &mut usize,
    param_count: usize,
    previous_types: &[u8],
) -> Result<(Vec<SqlParam>, Vec<u8>), String> {
    if param_count == 0 {
        return Ok((Vec::new(), Vec::new()));
    }

    let null_bitmap_len = param_count.div_ceil(8);
    let null_bitmap = take(payload, idx, null_bitmap_len)?.to_vec();
    let new_params_bound = *take(payload, idx, 1)?
        .first()
        .ok_or_else(|| "missing new params bound flag".to_string())?;

    let param_types = if new_params_bound == 1 {
        let mut types = Vec::with_capacity(param_count);
        for _ in 0..param_count {
            let ty = *take(payload, idx, 1)?
                .first()
                .ok_or_else(|| "missing param type".to_string())?;
            let _unsigned = take(payload, idx, 1)?;
            types.push(ty);
        }
        types
    } else if previous_types.len() == param_count {
        previous_types.to_vec()
    } else {
        return Err("COM_STMT_EXECUTE missing parameter types".into());
    };

    let mut params = Vec::with_capacity(param_count);
    for (param_idx, ty) in param_types.iter().enumerate() {
        if null_bitmap[param_idx / 8] & (1 << (param_idx % 8)) != 0 {
            params.push(SqlParam::Null);
        } else {
            params.push(parse_binary_param(payload, idx, *ty)?);
        }
    }
    Ok((params, param_types))
}

fn parse_binary_param(payload: &[u8], idx: &mut usize, ty: u8) -> Result<SqlParam, String> {
    match ty {
        MYSQL_TYPE_TINY => {
            let value = *take(payload, idx, 1)?
                .first()
                .ok_or_else(|| "missing tinyint parameter".to_string())?;
            Ok(SqlParam::Integer(value as i8 as i64))
        }
        MYSQL_TYPE_SHORT => Ok(SqlParam::Integer(get_u16_le(payload, idx)? as i16 as i64)),
        MYSQL_TYPE_LONG => Ok(SqlParam::Integer(get_u32_le(payload, idx)? as i32 as i64)),
        MYSQL_TYPE_LONGLONG => Ok(SqlParam::Integer(get_u64_le(payload, idx)? as i64)),
        MYSQL_TYPE_FLOAT => {
            let value = f32::from_bits(get_u32_le(payload, idx)?);
            Ok(SqlParam::Float(value as f64))
        }
        MYSQL_TYPE_DOUBLE => Ok(SqlParam::Float(f64::from_bits(get_u64_le(payload, idx)?))),
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_JSON => {
            let value = String::from_utf8(get_lenenc_bytes(payload, idx)?)
                .map_err(|e| format!("invalid string parameter: {e}"))?;
            Ok(SqlParam::Text(value))
        }
        MYSQL_TYPE_BLOB => Ok(SqlParam::Bytes(get_lenenc_bytes(payload, idx)?)),
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            Ok(SqlParam::Text(parse_binary_datetime(payload, idx)?))
        }
        MYSQL_TYPE_TIME => Ok(SqlParam::Text(parse_binary_time(payload, idx)?)),
        other => Err(format!(
            "unsupported MySQL prepared parameter type: {other}"
        )),
    }
}

fn parse_binary_datetime(payload: &[u8], idx: &mut usize) -> Result<String, String> {
    let len = *take(payload, idx, 1)?
        .first()
        .ok_or_else(|| "missing datetime length".to_string())? as usize;
    if len == 0 {
        return Ok("0000-00-00".to_string());
    }
    if len != 4 && len != 7 && len != 11 {
        return Err(format!("invalid datetime parameter length: {len}"));
    }
    let year = get_u16_le(payload, idx)?;
    let month = take(payload, idx, 1)?[0];
    let day = take(payload, idx, 1)?[0];
    if len == 4 {
        return Ok(format!("{year:04}-{month:02}-{day:02}"));
    }
    let hour = take(payload, idx, 1)?[0];
    let minute = take(payload, idx, 1)?[0];
    let second = take(payload, idx, 1)?[0];
    if len == 7 {
        return Ok(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
        ));
    }
    let micros = get_u32_le(payload, idx)?;
    Ok(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
    ))
}

fn parse_binary_time(payload: &[u8], idx: &mut usize) -> Result<String, String> {
    let len = take(payload, idx, 1)?[0] as usize;
    if len == 0 {
        return Ok("00:00:00".to_string());
    }
    if len != 8 && len != 12 {
        return Err(format!("invalid time parameter length: {len}"));
    }
    let negative = take(payload, idx, 1)?[0] != 0;
    let days = get_u32_le(payload, idx)?;
    let hours = take(payload, idx, 1)?[0] as u32 + days * 24;
    let minutes = take(payload, idx, 1)?[0];
    let seconds = take(payload, idx, 1)?[0];
    let sign = if negative { "-" } else { "" };
    if len == 8 {
        return Ok(format!("{sign}{hours:02}:{minutes:02}:{seconds:02}"));
    }
    let micros = get_u32_le(payload, idx)?;
    Ok(format!(
        "{sign}{hours:02}:{minutes:02}:{seconds:02}.{micros:06}"
    ))
}

#[derive(Debug, Clone, Copy)]
enum ScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
}

fn next_char_index(sql: &str, idx: usize) -> usize {
    idx + sql[idx..].chars().next().map(char::len_utf8).unwrap_or(1)
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Null,
    Text(String),
    Bool(bool),
    Integer(i64),
    Float(f64),
    FloatArray(Vec<f32>),
    Bytes(Vec<u8>),
}

pub fn sql_placeholder_count(sql: &str) -> Result<usize, String> {
    scan_sql_params(sql, None).map(|(_, count)| count)
}

pub(super) fn bind_sql_params(sql: &str, params: &[SqlParam]) -> Result<String, String> {
    scan_sql_params(sql, Some(params)).map(|(sql, _)| sql)
}

pub(super) fn scan_sql_params(
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
                    && bytes.get(idx + 1).is_some_and(|byte| byte.is_ascii_digit())
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
                        let param = params
                            .get(param_number - 1)
                            .ok_or_else(|| format!("missing SQL parameter: ${param_number}"))?;
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
pub(super) enum SqlScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
}

pub(super) fn next_char_index(sql: &str, idx: usize) -> usize {
    idx + sql[idx..].chars().next().map(char::len_utf8).unwrap_or(1)
}

pub(super) fn sql_param_literal(param: &SqlParam) -> Result<String, String> {
    match param {
        SqlParam::Null => Ok("NULL".to_string()),
        SqlParam::Text(value) => Ok(sql_string_literal(value)),
        SqlParam::Bool(value) => Ok(if *value { "true" } else { "false" }.to_string()),
        SqlParam::Integer(value) => Ok(value.to_string()),
        SqlParam::Float(value) => {
            if value.is_finite() {
                Ok(value.to_string())
            } else {
                Err(format!(
                    "non-finite SQL parameter is not supported: {value}"
                ))
            }
        }
        SqlParam::FloatArray(values) => {
            if values.is_empty() {
                return Err("FLOAT array SQL parameter cannot be empty".into());
            }
            if let Some(value) = values.iter().find(|value| !value.is_finite()) {
                return Err(format!(
                    "non-finite FLOAT array SQL parameter is not supported: {value}"
                ));
            }
            Ok(format!(
                "[{}]::FLOAT[]",
                values
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ))
        }
        SqlParam::Bytes(value) => Ok(format!("'\\x{}'", hex_encode(value))),
    }
}

pub(super) fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

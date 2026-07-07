fn parse_managed_partition_create(sql: &str) -> Result<Option<ManagedPartitionCreate>, String> {
    if !looks_like_managed_partition_create(sql) {
        return Ok(None);
    }

    let partition_idx = find_keyword_phrase(sql, "partition by range")
        .ok_or_else(|| "PARTITION BY RANGE clause is required".to_string())?;
    let mut cursor = partition_idx + "partition by range".len();
    cursor = skip_ascii_ws(sql, cursor);
    if !sql[cursor..].starts_with('(') {
        return Err("PARTITION BY RANGE requires a single parenthesized column".into());
    }
    let (partition_key_text, after_partition_key) = parse_parenthesized_segment(sql, cursor)?;
    let partition_key = parse_simple_identifier_text(&partition_key_text)?;

    let with_idx = find_keyword_phrase_from(sql, "with", after_partition_key)
        .ok_or_else(|| "managed partitioned table requires WITH options".to_string())?;
    let mut with_cursor = with_idx + "with".len();
    with_cursor = skip_ascii_ws(sql, with_cursor);
    if !sql[with_cursor..].starts_with('(') {
        return Err("managed partitioned table WITH options must be parenthesized".into());
    }
    let (options_text, after_options) = parse_parenthesized_segment(sql, with_cursor)?;
    let trailing = sql[after_options..].trim();
    if !trailing.is_empty() && trailing != ";" {
        return Err(format!(
            "unexpected text after managed partition options: {trailing}"
        ));
    }

    let (partition_unit, retention_count) = parse_partition_options(&options_text)?;
    let base_sql = sql[..partition_idx]
        .trim_end()
        .trim_end_matches(';')
        .to_string();
    Ok(Some(ManagedPartitionCreate {
        base_sql,
        partition_key,
        partition_unit,
        retention_count,
    }))
}

fn parse_partition_options(options_text: &str) -> Result<(String, i32), String> {
    let mut partition_unit = None;
    let mut retention = None;
    for option in split_top_level_commas(options_text) {
        let Some((key, value)) = split_key_value(&option) else {
            return Err(format!("invalid managed partition option: {option}"));
        };
        let key = parse_simple_identifier_text(key)?.to_ascii_lowercase();
        let value = parse_option_value(value)?;
        match key.as_str() {
            "partition_unit" => {
                if partition_unit.replace(value).is_some() {
                    return Err("duplicate partition_unit option".into());
                }
            }
            "retention" => {
                if retention.replace(value).is_some() {
                    return Err("duplicate retention option".into());
                }
            }
            _ => return Err(format!("unsupported managed partition option: {key}")),
        }
    }

    let partition_unit = partition_unit
        .ok_or_else(|| "managed partitioned table requires partition_unit".to_string())?;
    if !matches!(partition_unit.as_str(), "hour" | "day" | "month" | "year") {
        return Err(format!(
            "partition_unit must be one of hour, day, month, year: {partition_unit}"
        ));
    }
    let retention_text =
        retention.ok_or_else(|| "managed partitioned table requires retention".to_string())?;
    let retention_count: i32 = retention_text
        .parse()
        .map_err(|_| format!("retention must be a positive integer: {retention_text}"))?;
    if retention_count <= 0 {
        return Err(format!(
            "retention must be a positive integer: {retention_text}"
        ));
    }
    Ok((partition_unit, retention_count))
}

fn parse_simple_identifier_text(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty identifier".into());
    }
    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        return Ok(value[1..value.len() - 1].replace("\"\"", "\""));
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Ok(value.to_string())
    } else {
        Err(format!("expected a single identifier, got: {value}"))
    }
}

fn parse_option_value(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    parse_simple_identifier_text(value)
}

fn split_top_level_commas(value: &str) -> Vec<String> {
    split_top_level(value, ',')
}

fn split_key_value(value: &str) -> Option<(&str, &str)> {
    let idx = find_top_level_char(value, '=')?;
    Some((&value[..idx], &value[idx + 1..]))
}

fn parse_parenthesized_segment(sql: &str, open_idx: usize) -> Result<(String, usize), String> {
    let bytes = sql.as_bytes();
    if bytes.get(open_idx) != Some(&b'(') {
        return Err("expected '('".into());
    }
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut idx = open_idx;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_single {
            if byte == b'\'' {
                if bytes.get(idx + 1) == Some(&b'\'') {
                    idx += 2;
                    continue;
                }
                in_single = false;
            }
            idx += 1;
            continue;
        }
        if in_double {
            if byte == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    idx += 2;
                    continue;
                }
                in_double = false;
            }
            idx += 1;
            continue;
        }
        match byte {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((sql[open_idx + 1..idx].to_string(), idx + 1));
                }
            }
            _ => {}
        }
        idx += 1;
    }
    Err("unclosed parenthesized segment".into())
}

fn split_top_level(value: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = value.as_bytes();
    let delimiter = delimiter as u8;
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_single {
            if byte == b'\'' {
                if bytes.get(idx + 1) == Some(&b'\'') {
                    idx += 2;
                    continue;
                }
                in_single = false;
            }
            idx += 1;
            continue;
        }
        if in_double {
            if byte == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    idx += 2;
                    continue;
                }
                in_double = false;
            }
            idx += 1;
            continue;
        }
        match byte {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if byte == delimiter && depth == 0 => {
                parts.push(value[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    parts.push(value[start..].trim().to_string());
    parts.into_iter().filter(|part| !part.is_empty()).collect()
}

fn find_top_level_char(value: &str, target: char) -> Option<usize> {
    let bytes = value.as_bytes();
    let target = target as u8;
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_single {
            if byte == b'\'' {
                if bytes.get(idx + 1) == Some(&b'\'') {
                    idx += 2;
                    continue;
                }
                in_single = false;
            }
            idx += 1;
            continue;
        }
        if in_double {
            if byte == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    idx += 2;
                    continue;
                }
                in_double = false;
            }
            idx += 1;
            continue;
        }
        match byte {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if byte == target && depth == 0 => return Some(idx),
            _ => {}
        }
        idx += 1;
    }
    None
}

fn find_keyword_phrase(sql: &str, phrase: &str) -> Option<usize> {
    find_keyword_phrase_from(sql, phrase, 0)
}

fn find_keyword_phrase_from(sql: &str, phrase: &str, start: usize) -> Option<usize> {
    let lower = sql.to_ascii_lowercase();
    let phrase = phrase.to_ascii_lowercase();
    let bytes = sql.as_bytes();
    let lower_bytes = lower.as_bytes();
    let phrase_bytes = phrase.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut idx = start.min(bytes.len());
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_single {
            if byte == b'\'' {
                if bytes.get(idx + 1) == Some(&b'\'') {
                    idx += 2;
                    continue;
                }
                in_single = false;
            }
            idx += 1;
            continue;
        }
        if in_double {
            if byte == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    idx += 2;
                    continue;
                }
                in_double = false;
            }
            idx += 1;
            continue;
        }
        match byte {
            b'\'' => {
                in_single = true;
                idx += 1;
                continue;
            }
            b'"' => {
                in_double = true;
                idx += 1;
                continue;
            }
            _ => {}
        }
        let end = idx + phrase_bytes.len();
        if end <= lower_bytes.len()
            && &lower_bytes[idx..end] == phrase_bytes
            && is_keyword_boundary(bytes, idx, end)
        {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

fn is_keyword_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
    let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
    before_ok && after_ok
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn skip_ascii_ws(sql: &str, mut idx: usize) -> usize {
    let bytes = sql.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

fn parse_one_statement(sql: &str) -> Result<(Statement, String), String> {
    let normalized = sql.trim_start().to_ascii_lowercase();
    let statements = if normalized.starts_with("comment on ") {
        let dialect = PostgreSqlDialect {};
        Parser::parse_sql(&dialect, sql)
    } else {
        let dialect = DuckDbDialect {};
        Parser::parse_sql(&dialect, sql)
    }
    .map_err(|e| format!("catalog sql parse failed: {e}"))?;
    if statements.len() != 1 {
        return Err(format!(
            "only one SQL statement is supported, got {}",
            statements.len()
        ));
    }
    let statement = statements.into_iter().next().expect("statement exists");
    let normalized_sql = statement.to_string();
    Ok((statement, normalized_sql))
}


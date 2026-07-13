use super::*;

pub(super) fn type_id_for_duckdb_type(duckdb_type: &str) -> Result<i64, String> {
    scalar_type_id_for_duckdb_type(duckdb_type)
        .ok_or_else(|| format!("unsupported DuckDB type for rsduck catalog: {duckdb_type}"))
}

pub(super) fn ensure_type_id_for_duckdb_type(
    conn: &Connection,
    duckdb_type: &str,
) -> Result<i64, String> {
    if let Some(type_id) = scalar_type_id_for_duckdb_type(duckdb_type) {
        return Ok(type_id);
    }
    validate_supported_complex_type(duckdb_type)?;
    if let Some(existing) = lookup_physical_type_id(conn, duckdb_type)? {
        return Ok(existing);
    }
    let type_id = allocate_oid(conn)?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_type(oid, typname, typnamespace, typowner, typlen, \
             typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type) \
             VALUES ({type_id}, '{}', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'J', TRUE, 0, 0, 0, '{}')",
            sql_string(&complex_typname(duckdb_type)),
            sql_string(duckdb_type)
        ),
        [],
    )
    .map_err(|e| format!("register complex DuckDB type {duckdb_type} failed: {e}"))?;
    Ok(type_id)
}

fn scalar_type_id_for_duckdb_type(duckdb_type: &str) -> Option<i64> {
    let lower = duckdb_type.to_ascii_lowercase();
    if lower == "boolean" || lower == "bool" {
        Some(TYPE_BOOL)
    } else if lower == "smallint" || lower == "int2" {
        Some(TYPE_INT2)
    } else if lower == "integer" || lower == "int" || lower == "int4" {
        Some(TYPE_INT4)
    } else if lower == "bigint" || lower == "int8" {
        Some(TYPE_INT8)
    } else if lower == "real" || lower == "float" || lower == "float4" {
        Some(TYPE_FLOAT4)
    } else if lower == "double" || lower == "double precision" || lower == "float8" {
        Some(TYPE_FLOAT8)
    } else if lower.starts_with("decimal") || lower.starts_with("numeric") {
        Some(TYPE_NUMERIC)
    } else if lower == "varchar" || lower.starts_with("varchar(") {
        Some(TYPE_VARCHAR)
    } else if lower == "text" || lower == "string" {
        Some(TYPE_TEXT)
    } else if lower == "date" {
        Some(TYPE_DATE)
    } else if lower == "time" || lower.starts_with("time(") {
        Some(TYPE_TIME)
    } else if lower.starts_with("timestamp") || lower == "datetime" {
        Some(TYPE_TIMESTAMP)
    } else {
        None
    }
}

pub(super) fn is_complex_duckdb_type(duckdb_type: &str) -> bool {
    scalar_type_id_for_duckdb_type(duckdb_type).is_none()
        && validate_supported_complex_type(duckdb_type).is_ok()
}

pub(super) fn validate_supported_column_type(duckdb_type: &str) -> Result<(), String> {
    if scalar_type_id_for_duckdb_type(duckdb_type).is_some() {
        return Ok(());
    }
    validate_supported_complex_type(duckdb_type)
}

fn lookup_physical_type_id(conn: &Connection, duckdb_type: &str) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT oid FROM rsduck_catalog.rs_type WHERE rsduck_physical_type = '{}'",
            sql_string(duckdb_type)
        ))
        .map_err(|e| format!("prepare complex type lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query complex type lookup failed: {e}"))?;
    if let Some(row) = rows
        .next()
        .map_err(|e| format!("read complex type lookup failed: {e}"))?
    {
        Ok(Some(row.get(0).map_err(|e| {
            format!("read complex type oid failed: {e}")
        })?))
    } else {
        Ok(None)
    }
}

fn complex_typname(duckdb_type: &str) -> String {
    let suffix = duckdb_type
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    format!("complex_{suffix}")
}

fn validate_supported_complex_type(duckdb_type: &str) -> Result<(), String> {
    let trimmed = duckdb_type.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(element) = trimmed.strip_suffix("[]") {
        if element.ends_with("[]") || contains_complex_type_marker(element) {
            return Err(format!(
                "nested complex DuckDB types are not supported by rsduck catalog: {duckdb_type}"
            ));
        }
        return validate_scalar_type_part(element)
            .map_err(|_| format!("unsupported DuckDB type for rsduck catalog: {duckdb_type}"));
    }
    if trimmed.ends_with(']') {
        return validate_fixed_float_array_type(trimmed);
    }
    if lower.starts_with("struct(") && trimmed.ends_with(')') {
        let inner = &trimmed["STRUCT".len() + 1..trimmed.len() - 1];
        let fields = split_top_level(inner, ',')?;
        if fields.is_empty() {
            return Err("STRUCT complex type must contain at least one field".into());
        }
        for field in fields {
            let (_, field_type) = split_struct_field(&field)?;
            if contains_complex_type_marker(field_type) {
                return Err(format!(
                    "nested complex DuckDB types are not supported by rsduck catalog: {duckdb_type}"
                ));
            }
            validate_scalar_type_part(field_type).map_err(|_| {
                format!("unsupported STRUCT field type for rsduck catalog: {field_type}")
            })?;
        }
        return Ok(());
    }
    if lower.starts_with("map(") && trimmed.ends_with(')') {
        let inner = &trimmed["MAP".len() + 1..trimmed.len() - 1];
        let parts = split_top_level(inner, ',')?;
        if parts.len() != 2 {
            return Err(format!(
                "MAP complex type must specify key and value types: {duckdb_type}"
            ));
        }
        for part in parts {
            if contains_complex_type_marker(&part) {
                return Err(format!(
                    "nested complex DuckDB types are not supported by rsduck catalog: {duckdb_type}"
                ));
            }
            validate_scalar_type_part(&part).map_err(|_| {
                format!("unsupported MAP key/value type for rsduck catalog: {part}")
            })?;
        }
        return Ok(());
    }
    Err(format!(
        "unsupported DuckDB type for rsduck catalog: {duckdb_type}"
    ))
}

fn validate_fixed_float_array_type(duckdb_type: &str) -> Result<(), String> {
    fixed_float_array_dimension(duckdb_type).map(|_| ())
}

pub(super) fn fixed_float_array_dimension(duckdb_type: &str) -> Result<usize, String> {
    let Some(open_bracket) = duckdb_type.rfind('[') else {
        return Err(format!(
            "invalid fixed array DuckDB type syntax: {duckdb_type}"
        ));
    };
    let element = duckdb_type[..open_bracket].trim();
    let dimension_text = duckdb_type[open_bracket + 1..duckdb_type.len() - 1].trim();
    if element.is_empty()
        || element.contains('[')
        || element.contains(']')
        || dimension_text.is_empty()
        || !dimension_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!(
            "invalid fixed array DuckDB type syntax: {duckdb_type}"
        ));
    }
    if scalar_type_id_for_duckdb_type(element) != Some(TYPE_FLOAT4) {
        return Err(format!(
            "fixed array DuckDB type only supports FLOAT elements: {duckdb_type}"
        ));
    }
    let dimension = dimension_text
        .parse::<usize>()
        .map_err(|_| format!("invalid fixed array dimension: {duckdb_type}"))?;
    if dimension == 0 {
        return Err(format!(
            "fixed array dimension must be greater than zero: {duckdb_type}"
        ));
    }
    Ok(dimension)
}

fn validate_scalar_type_part(type_text: &str) -> Result<(), String> {
    if scalar_type_id_for_duckdb_type(type_text.trim()).is_some() {
        Ok(())
    } else {
        Err(format!("unsupported scalar type: {type_text}"))
    }
}

fn contains_complex_type_marker(type_text: &str) -> bool {
    let lower = type_text.trim().to_ascii_lowercase();
    lower.ends_with("[]")
        || (lower.ends_with(']') && lower.contains('['))
        || lower.starts_with("struct(")
        || lower.starts_with("map(")
        || lower.starts_with("list(")
        || lower.starts_with("list<")
}

fn split_top_level(input: &str, delimiter: char) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (idx, ch) = chars[i];
        if let Some(q) = quote {
            if ch == q {
                if i + 1 < chars.len() && chars[i + 1].1 == q {
                    i += 1;
                } else {
                    quote = None;
                }
            }
            i += 1;
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '(' | '<' => depth += 1,
            ')' | '>' => depth -= 1,
            _ if ch == delimiter && depth == 0 => {
                parts.push(input[start..idx].trim().to_string());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
        if depth < 0 {
            return Err(format!("invalid complex type syntax: {input}"));
        }
        i += 1;
    }
    if depth != 0 || quote.is_some() {
        return Err(format!("invalid complex type syntax: {input}"));
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    Ok(parts)
}

fn split_struct_field(field: &str) -> Result<(&str, &str), String> {
    let field = field.trim();
    if field.is_empty() {
        return Err("STRUCT field cannot be empty".into());
    }
    let mut chars = field.char_indices();
    if let Some((_, quote @ ('"' | '`'))) = chars.next() {
        let mut escaped = false;
        for (idx, ch) in field[quote.len_utf8()..].char_indices() {
            let absolute = idx + quote.len_utf8();
            if ch == quote {
                if escaped {
                    escaped = false;
                    continue;
                }
                let rest = field[absolute + quote.len_utf8()..].trim();
                if rest.is_empty() {
                    return Err(format!("STRUCT field is missing type: {field}"));
                }
                return Ok((&field[..absolute + quote.len_utf8()], rest));
            }
            escaped = ch == quote;
        }
        return Err(format!("invalid quoted STRUCT field: {field}"));
    }
    for (idx, ch) in field.char_indices() {
        if ch.is_whitespace() {
            let name = field[..idx].trim();
            let ty = field[idx..].trim();
            if name.is_empty() || ty.is_empty() {
                return Err(format!("invalid STRUCT field: {field}"));
            }
            return Ok((name, ty));
        }
    }
    Err(format!("STRUCT field is missing type: {field}"))
}

pub(super) fn duckdb_type_for_type_id(conn: &Connection, type_id: i64) -> Result<String, String> {
    conn.query_row(
        &format!("SELECT rsduck_physical_type FROM rsduck_catalog.rs_type WHERE oid = {type_id}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("lookup DuckDB type for catalog type id {type_id} failed: {e}"))
}

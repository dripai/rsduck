fn catalog_scalar_function_sql(raw_sql: &str, normalized_sql: &str) -> Option<String> {
    if normalized_sql.contains(" from ") {
        return None;
    }

    let raw_body = strip_select(raw_sql)?;
    let normalized_body = strip_select(normalized_sql)?;

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "format_type") {
        let oid = parse_i64_arg(args.first()?)?;
        return Some(format!(
            "SELECT COALESCE((SELECT typname FROM rsduck_catalog.pg_type WHERE oid = {oid}), 'unknown') AS format_type"
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "pg_get_expr") {
        let expr = unquote_sql_literal(args.first()?.trim())?;
        return Some(format!(
            "SELECT '{}' AS pg_get_expr",
            sql_string_literal(&expr)
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "pg_get_constraintdef") {
        let oid = parse_i64_arg(args.first()?)?;
        return Some(format!(
            "SELECT {} AS pg_get_constraintdef",
            pg_get_constraintdef_expr(&oid.to_string())
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "obj_description") {
        let oid = parse_i64_arg(args.first()?)?;
        return Some(format!(
            "SELECT COALESCE((SELECT description FROM rsduck_catalog.pg_description WHERE objoid = {oid} AND objsubid = 0 ORDER BY classoid LIMIT 1), '') AS obj_description"
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "col_description") {
        let oid = parse_i64_arg(args.first()?)?;
        let attnum = parse_i64_arg(args.get(1)?)?;
        return Some(format!(
            "SELECT COALESCE((SELECT description FROM rsduck_catalog.pg_description WHERE objoid = {oid} AND objsubid = {attnum} LIMIT 1), '') AS col_description"
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "pg_table_is_visible") {
        let oid = parse_i64_arg(args.first()?)?;
        return Some(format!(
            "
            SELECT CASE WHEN EXISTS (
                SELECT 1
                FROM rsduck_catalog.pg_class c
                JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
                JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
                WHERE c.oid = {oid}
                  AND c.status = 'active'
                  AND ext.visibility = 'user'
                  AND n.nspname = 'main'
            ) THEN 't' ELSE 'f' END AS pg_table_is_visible
            "
        ));
    }

    if let Some(args) = scalar_function_args(raw_body, normalized_body, "pg_get_userbyid") {
        let oid = parse_i64_arg(args.first()?)?;
        return Some(format!(
            "SELECT COALESCE((SELECT username FROM rsduck_catalog.rs_user WHERE user_id = {oid}), 'unknown') AS pg_get_userbyid"
        ));
    }

    None
}

fn strip_select(sql: &str) -> Option<&str> {
    sql.trim()
        .trim_end_matches(';')
        .trim()
        .strip_prefix("select ")
        .or_else(|| {
            let value = sql.trim().trim_end_matches(';').trim();
            value
                .get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("select "))
                .and_then(|_| value.get(7..))
        })
        .map(str::trim)
}

fn scalar_function_args(
    raw_body: &str,
    normalized_body: &str,
    function_name: &str,
) -> Option<Vec<String>> {
    let direct = format!("{function_name}(");
    let qualified = format!("pg_catalog.{function_name}(");
    let open_idx = if normalized_body.starts_with(&direct) {
        direct.len() - 1
    } else if normalized_body.starts_with(&qualified) {
        qualified.len() - 1
    } else {
        return None;
    };
    let close_idx = find_closing_paren(raw_body, open_idx)?;
    Some(split_function_args(&raw_body[open_idx + 1..close_idx]))
}

fn find_closing_paren(value: &str, open_idx: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut depth = 0_i32;
    let mut in_single = false;
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
        match byte {
            b'\'' => in_single = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn split_function_args(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    let mut args = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    let mut in_single = false;
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
        match byte {
            b'\'' => in_single = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                args.push(value[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    args.push(value[start..].trim().to_string());
    args
}

fn parse_i64_arg(value: &str) -> Option<i64> {
    value.trim().trim_matches('\'').parse().ok()
}

fn unquote_sql_literal(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

fn sql_string_literal(value: &str) -> String {
    value.replace('\'', "''")
}


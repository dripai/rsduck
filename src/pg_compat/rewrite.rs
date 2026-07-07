fn rewrite_catalog_relation_references(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut last = 0;
    let mut replaced = false;
    let mut in_single = false;
    let mut in_double = false;

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

        let relation_start = if let Some(keyword_len) = if keyword_at(sql, idx, "from") {
            Some(4)
        } else if keyword_at(sql, idx, "join") {
            Some(4)
        } else {
            None
        } {
            skip_ascii_ws(sql, idx + keyword_len)
        } else if byte == b'(' {
            skip_ascii_ws(sql, idx + 1)
        } else {
            idx += 1;
            continue;
        };
        let Some((relation_key, relation_end)) = parse_relation_reference(sql, relation_start)
        else {
            idx += 1;
            continue;
        };
        let Some(projection_sql) = catalog_projection_sql(&relation_key, sql) else {
            idx += 1;
            continue;
        };

        output.push_str(&sql[last..relation_start]);
        output.push('(');
        output.push_str(&projection_sql);
        output.push(')');
        last = relation_end;
        idx = relation_end;
        replaced = true;
    }

    if replaced {
        output.push_str(&sql[last..]);
        Some(output)
    } else {
        None
    }
}

fn rewrite_catalog_function_calls(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut last = 0;
    let mut replaced = false;
    let mut in_single = false;
    let mut in_double = false;

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

        let Some((function_name, open_idx)) = catalog_function_at(sql, idx) else {
            idx += 1;
            continue;
        };
        let Some(close_idx) = find_closing_paren(sql, open_idx) else {
            idx += 1;
            continue;
        };
        let args = split_function_args(&sql[open_idx + 1..close_idx]);
        let Some(replacement) = catalog_function_expr(function_name, &args) else {
            idx += 1;
            continue;
        };
        output.push_str(&sql[last..idx]);
        output.push_str(&replacement);
        last = close_idx + 1;
        idx = close_idx + 1;
        replaced = true;
    }

    if replaced {
        output.push_str(&sql[last..]);
        Some(output)
    } else {
        None
    }
}

fn rewrite_pg_any_membership(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut idx = 0;
    let mut last = 0;
    let mut replaced = false;
    let mut in_single = false;
    let mut in_double = false;

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

        if !keyword_at(sql, idx, "any") {
            idx += 1;
            continue;
        }
        let open_idx = skip_ascii_ws(sql, idx + 3);
        if bytes.get(open_idx) != Some(&b'(') {
            idx += 1;
            continue;
        }
        let Some(close_idx) = find_closing_paren(sql, open_idx) else {
            idx += 1;
            continue;
        };
        let any_arg = sql[open_idx + 1..close_idx].trim();
        if !is_constraint_key_expr(any_arg) {
            idx = close_idx + 1;
            continue;
        }

        let Some((lhs_start, lhs_end)) = lhs_for_any_equality(sql, idx) else {
            idx = close_idx + 1;
            continue;
        };
        let lhs = sql[lhs_start..lhs_end].trim();
        output.push_str(&sql[last..lhs_start]);
        output.push_str(&format!(
            "COALESCE(list_position(string_split(CAST({any_arg} AS VARCHAR), ','), CAST({lhs} AS VARCHAR)), 0) > 0"
        ));
        last = close_idx + 1;
        idx = close_idx + 1;
        replaced = true;
    }

    if replaced {
        output.push_str(&sql[last..]);
        Some(output)
    } else {
        None
    }
}

fn rewrite_pg_type_casts(sql: &str) -> Option<String> {
    let rewritten = replace_ignore_ascii_case(sql, "::information_schema.character_data", "::VARCHAR");
    let rewritten = replace_ignore_ascii_case(&rewritten, "'pg_class'::regclass::oid", "'1259'");
    let rewritten = replace_ignore_ascii_case(
        &rewritten,
        "'pg_catalog.pg_class'::regclass::oid",
        "'1259'",
    );
    (rewritten != sql).then_some(rewritten)
}

fn replace_ignore_ascii_case(input: &str, needle: &str, replacement: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        let end = idx + needle.len();
        if end <= input.len() && input[idx..end].eq_ignore_ascii_case(needle) {
            output.push_str(replacement);
            idx = end;
        } else {
            let ch = input[idx..].chars().next().expect("valid char boundary");
            output.push(ch);
            idx += ch.len_utf8();
        }
    }
    output
}

fn lhs_for_any_equality(sql: &str, any_idx: usize) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    let mut eq_idx = any_idx.checked_sub(1)?;
    while eq_idx > 0 && bytes[eq_idx].is_ascii_whitespace() {
        eq_idx -= 1;
    }
    if bytes.get(eq_idx) != Some(&b'=') {
        return None;
    }

    let mut lhs_end = eq_idx;
    while lhs_end > 0 && bytes[lhs_end - 1].is_ascii_whitespace() {
        lhs_end -= 1;
    }
    let mut lhs_start = lhs_end;
    while lhs_start > 0 {
        let byte = bytes[lhs_start - 1];
        if is_ident_byte(byte) || byte == b'.' {
            lhs_start -= 1;
        } else {
            break;
        }
    }
    (lhs_start < lhs_end).then_some((lhs_start, lhs_end))
}

fn is_constraint_key_expr(expr: &str) -> bool {
    let normalized = expr.trim().trim_matches('"').to_ascii_lowercase();
    normalized == "conkey"
        || normalized == "confkey"
        || normalized.ends_with(".conkey")
        || normalized.ends_with(".confkey")
}

fn catalog_function_at<'a>(sql: &'a str, idx: usize) -> Option<(&'a str, usize)> {
    for function_name in [
        "format_type",
        "pg_get_expr",
        "pg_get_constraintdef",
        "pg_get_userbyid",
        "obj_description",
        "col_description",
        "pg_table_is_visible",
    ] {
        for prefix in ["", "pg_catalog."] {
            let pattern = format!("{prefix}{function_name}(");
            let end = idx + pattern.len();
            if end <= sql.len()
                && sql[idx..end].eq_ignore_ascii_case(&pattern)
                && (idx == 0 || !is_ident_byte(sql.as_bytes()[idx - 1]))
            {
                return Some((function_name, end - 1));
            }
        }
    }
    None
}

fn catalog_function_expr(function_name: &str, args: &[String]) -> Option<String> {
    let first_arg = args.first()?.trim();
    match function_name {
        "format_type" => Some(format!(
            "COALESCE((SELECT typname FROM rsduck_catalog.pg_type WHERE oid = CAST({first_arg} AS BIGINT)), 'unknown')"
        )),
        "pg_get_expr" => Some(format!("CAST({first_arg} AS VARCHAR)")),
        "pg_get_constraintdef" => Some(pg_get_constraintdef_expr(first_arg)),
        "pg_get_userbyid" => Some(format!(
            "COALESCE((SELECT username FROM rsduck_catalog.rs_user WHERE user_id = CAST({first_arg} AS BIGINT)), 'unknown')"
        )),
        "obj_description" => Some(format!(
            "COALESCE((SELECT description FROM rsduck_catalog.pg_description WHERE objoid = CAST({first_arg} AS BIGINT) AND objsubid = 0 ORDER BY classoid LIMIT 1), '')"
        )),
        "col_description" => {
            let attnum = args.get(1)?.trim();
            Some(format!(
                "COALESCE((SELECT description FROM rsduck_catalog.pg_description WHERE objoid = CAST({first_arg} AS BIGINT) AND objsubid = CAST({attnum} AS INTEGER) LIMIT 1), '')"
            ))
        }
        "pg_table_is_visible" => Some(format!(
            "CASE WHEN EXISTS (
                SELECT 1
                FROM rsduck_catalog.pg_class c
                JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
                JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
                WHERE c.oid = CAST({first_arg} AS BIGINT)
                  AND c.status = 'active'
                  AND ext.visibility = 'user'
                  AND n.nspname = 'main'
            ) THEN 't' ELSE 'f' END"
        )),
        _ => None,
    }
}

fn pg_get_constraintdef_expr(oid_expr: &str) -> String {
    format!(
        "
        COALESCE((
            SELECT CASE target_con.contype
                WHEN 'p' THEN 'PRIMARY KEY (' || COALESCE((
                    SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                    FROM rsduck_catalog.pg_attribute a
                    WHERE a.attrelid = target_con.conrelid
                      AND a.attisdropped = FALSE
                      AND COALESCE(list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                ), '') || ')'
                WHEN 'u' THEN 'UNIQUE (' || COALESCE((
                    SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                    FROM rsduck_catalog.pg_attribute a
                    WHERE a.attrelid = target_con.conrelid
                      AND a.attisdropped = FALSE
                      AND COALESCE(list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                ), '') || ')'
                WHEN 'c' THEN 'CHECK (' || target_con.conbin || ')'
                WHEN 'f' THEN 'FOREIGN KEY (' || COALESCE((
                    SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                    FROM rsduck_catalog.pg_attribute a
                    WHERE a.attrelid = target_con.conrelid
                      AND a.attisdropped = FALSE
                      AND COALESCE(list_position(string_split(target_con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                ), '') || ') REFERENCES ' || COALESCE((
                    SELECT rn.nspname || '.' || rc.relname
                    FROM rsduck_catalog.pg_class rc
                    JOIN rsduck_catalog.pg_namespace rn ON rn.oid = rc.relnamespace
                    WHERE rc.oid = target_con.confrelid
                ), 'unknown') || ' (' || COALESCE((
                    SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(target_con.confkey, ','), CAST(a.attnum AS VARCHAR)))
                    FROM rsduck_catalog.pg_attribute a
                    WHERE a.attrelid = target_con.confrelid
                      AND a.attisdropped = FALSE
                      AND COALESCE(list_position(string_split(target_con.confkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                ), '') || ')'
                ELSE target_con.contype
            END
            FROM rsduck_catalog.pg_constraint target_con
            WHERE target_con.oid = CAST({oid_expr} AS BIGINT)
        ), 'unknown')
        "
    )
}

fn catalog_projection_sql(relation_key: &str, source_sql: &str) -> Option<String> {
    match relation_key {
        "pg_catalog.pg_attribute" => {
            Some(pg_attribute_sql(pg_attribute_includes_dropped(source_sql)))
        }
        "information_schema.columns" => Some(information_schema_columns_sql()),
        "pg_catalog.pg_index" => Some(pg_index_sql()),
        "pg_catalog.pg_inherits" => Some(pg_inherits_sql()),
        "pg_catalog.pg_constraint" => Some(pg_constraint_sql()),
        "information_schema.table_constraints" => Some(information_schema_table_constraints_sql()),
        "information_schema.key_column_usage" => Some(information_schema_key_column_usage_sql()),
        "information_schema.constraint_column_usage" => {
            Some(information_schema_constraint_column_usage_sql())
        }
        "pg_catalog.pg_attrdef" => Some(pg_attrdef_sql()),
        "pg_catalog.pg_depend" => Some(pg_depend_sql()),
        "pg_catalog.pg_description" => Some(pg_description_sql()),
        "pg_catalog.pg_views" => Some(pg_views_sql()),
        "information_schema.views" => Some(information_schema_views_sql()),
        "pg_catalog.pg_indexes" => Some(pg_indexes_sql()),
        "pg_catalog.pg_class" => Some(pg_class_sql()),
        "pg_catalog.pg_tables" => Some(pg_tables_sql()),
        "information_schema.tables" => Some(information_schema_tables_sql()),
        "pg_catalog.pg_namespace" => Some(pg_namespace_sql()),
        "information_schema.schemata" => Some(information_schema_schemata_sql()),
        "pg_catalog.pg_type" => Some(pg_type_sql()),
        "pg_catalog.pg_database" => Some(pg_database_sql()),
        "pg_catalog.pg_user" => Some(pg_user_sql()),
        "pg_catalog.pg_roles" | "pg_catalog.pg_authid" => Some(pg_roles_sql()),
        "pg_catalog.pg_settings" => Some(pg_settings_sql()),
        "pg_catalog.pg_proc" => Some(pg_proc_sql()),
        "pg_catalog.pg_tablespace" => Some(pg_tablespace_sql()),
        "pg_catalog.pg_collation" => Some(pg_collation_sql()),
        "pg_catalog.pg_sequence" => Some(pg_sequence_sql()),
        "pg_catalog.pg_foreign_table" => empty_pg_catalog_sql(" from pg_foreign_table"),
        "pg_catalog.pg_foreign_server" => empty_pg_catalog_sql(" from pg_foreign_server"),
        "pg_catalog.pg_trigger" => empty_pg_catalog_sql(" from pg_trigger"),
        "pg_catalog.pg_extension" => empty_pg_catalog_sql(" from pg_extension"),
        "pg_catalog.pg_policy" => empty_pg_catalog_sql(" from pg_policy"),
        "pg_catalog.pg_matviews" => empty_pg_catalog_sql(" from pg_matviews"),
        "pg_catalog.pg_sequences" => empty_pg_catalog_sql(" from pg_sequences"),
        _ => None,
    }
}

fn pg_attribute_includes_dropped(sql: &str) -> bool {
    normalize_sql(sql).contains("attisdropped")
}

fn parse_relation_reference(sql: &str, start: usize) -> Option<(String, usize)> {
    let mut idx = start;
    let mut parts = Vec::new();
    loop {
        let (part, next_idx) = parse_identifier_part(sql, idx)?;
        parts.push(part.to_ascii_lowercase());
        idx = skip_ascii_ws(sql, next_idx);
        if sql.as_bytes().get(idx) != Some(&b'.') {
            break;
        }
        idx = skip_ascii_ws(sql, idx + 1);
    }
    let key = match parts.as_slice() {
        [relation] if relation.starts_with("pg_") => format!("pg_catalog.{relation}"),
        [schema, relation] => format!("{schema}.{relation}"),
        _ => return None,
    };
    Some((key, idx))
}

fn parse_identifier_part(sql: &str, start: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    if bytes.get(start) == Some(&b'"') {
        let mut idx = start + 1;
        let mut value = String::new();
        while idx < bytes.len() {
            if bytes[idx] == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    value.push('"');
                    idx += 2;
                    continue;
                }
                return Some((value, idx + 1));
            }
            value.push(bytes[idx] as char);
            idx += 1;
        }
        return None;
    }

    let mut idx = start;
    while idx < bytes.len() && is_ident_byte(bytes[idx]) {
        idx += 1;
    }
    if idx == start {
        None
    } else {
        Some((sql[start..idx].to_string(), idx))
    }
}

fn keyword_at(sql: &str, idx: usize, keyword: &str) -> bool {
    let bytes = sql.as_bytes();
    let end = idx + keyword.len();
    end <= bytes.len()
        && sql[idx..end].eq_ignore_ascii_case(keyword)
        && (idx == 0 || !is_ident_byte(bytes[idx - 1]))
        && (end == bytes.len() || !is_ident_byte(bytes[end]))
}

fn skip_ascii_ws(sql: &str, mut idx: usize) -> usize {
    let bytes = sql.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    idx
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn normalize_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

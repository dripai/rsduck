use crate::db::SqlResult;

const PG_NAMESPACE_CLASSOID: i64 = 2615;

pub fn compat_result(sql: &str, current_user: &str) -> Option<SqlResult> {
    let normalized = normalize_sql(sql);
    pg_set_result(&normalized)
        .or_else(|| pg_show_result(&normalized))
        .or_else(|| pg_scalar_result(&normalized, current_user))
        .or_else(|| pg_database_result(&normalized))
        .or_else(|| pg_settings_result(&normalized))
}

pub fn rewrite_sql(sql: &str) -> Option<String> {
    let normalized = normalize_sql(sql);
    if !normalized.starts_with("select ") && !normalized.starts_with("with ") {
        return None;
    }

    if let Some(sql) = catalog_scalar_function_sql(sql, &normalized) {
        return Some(sql);
    }

    rewrite_catalog_relation_references(sql)
}

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

        let keyword_len = if keyword_at(sql, idx, "from") {
            Some(4)
        } else if keyword_at(sql, idx, "join") {
            Some(4)
        } else {
            None
        };
        let Some(keyword_len) = keyword_len else {
            idx += 1;
            continue;
        };

        let relation_start = skip_ascii_ws(sql, idx + keyword_len);
        let Some((relation_key, relation_end)) = parse_relation_reference(sql, relation_start)
        else {
            idx += keyword_len;
            continue;
        };
        let Some(projection_sql) = catalog_projection_sql(&relation_key) else {
            idx += keyword_len;
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

fn catalog_projection_sql(relation_key: &str) -> Option<String> {
    match relation_key {
        "pg_catalog.pg_attribute" | "information_schema.columns" => Some(pg_attribute_sql()),
        "pg_catalog.pg_index" => Some(pg_index_sql()),
        "pg_catalog.pg_constraint" | "information_schema.table_constraints" => {
            Some(pg_constraint_sql())
        }
        "information_schema.key_column_usage" => Some(information_schema_key_column_usage_sql()),
        "information_schema.constraint_column_usage" => {
            Some(information_schema_constraint_column_usage_sql())
        }
        "pg_catalog.pg_attrdef" => Some(pg_attrdef_sql()),
        "pg_catalog.pg_depend" => Some(pg_depend_sql()),
        "pg_catalog.pg_description" => Some(pg_description_sql()),
        "pg_catalog.pg_views" | "information_schema.views" => Some(pg_views_sql()),
        "pg_catalog.pg_indexes" => Some(pg_indexes_sql()),
        "pg_catalog.pg_class" | "pg_catalog.pg_tables" | "information_schema.tables" => {
            Some(pg_class_sql())
        }
        "pg_catalog.pg_namespace" | "information_schema.schemata" => Some(pg_namespace_sql()),
        "pg_catalog.pg_type" => Some(pg_type_sql()),
        "pg_catalog.pg_user" => Some(pg_user_sql()),
        "pg_catalog.pg_roles" | "pg_catalog.pg_authid" => Some(pg_roles_sql()),
        "pg_catalog.pg_trigger" => empty_pg_catalog_sql(" from pg_trigger"),
        "pg_catalog.pg_proc" => empty_pg_catalog_sql(" from pg_proc"),
        "pg_catalog.pg_extension" => empty_pg_catalog_sql(" from pg_extension"),
        "pg_catalog.pg_policy" => empty_pg_catalog_sql(" from pg_policy"),
        "pg_catalog.pg_matviews" => empty_pg_catalog_sql(" from pg_matviews"),
        "pg_catalog.pg_sequences" => empty_pg_catalog_sql(" from pg_sequences"),
        _ => None,
    }
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

fn one_row(columns: &[&str], values: &[&str]) -> SqlResult {
    SqlResult::Query {
        columns: columns.iter().map(|v| (*v).to_string()).collect(),
        rows: vec![values.iter().map(|v| (*v).to_string()).collect()],
    }
}

fn exec_ok(command: &str) -> SqlResult {
    SqlResult::Execute {
        command: command.to_string(),
        affected_rows: 0,
    }
}

fn pg_set_result(sql: &str) -> Option<SqlResult> {
    let sql = sql.strip_prefix("set ")?;
    let supported = [
        "application_name",
        "client_encoding",
        "datestyle",
        "extra_float_digits",
        "search_path",
        "standard_conforming_strings",
        "timezone",
        "time zone",
        "transaction isolation level",
    ];
    supported
        .iter()
        .any(|name| sql.starts_with(name))
        .then(|| exec_ok("SET"))
}

fn pg_show_result(sql: &str) -> Option<SqlResult> {
    let setting = sql.strip_prefix("show ")?.trim();
    if setting == "all" {
        return Some(pg_settings_table_result());
    }

    let normalized_setting = match setting {
        "transaction isolation level" => "transaction_isolation",
        _ => setting,
    };
    let (column, value) = pg_setting(normalized_setting)?;
    Some(one_row(&[column], &[value]))
}

fn pg_scalar_result(sql: &str, current_user: &str) -> Option<SqlResult> {
    if let Some(result) = current_setting_result(sql) {
        return Some(result);
    }

    if !sql.starts_with("select ") || sql.contains(" from ") {
        return None;
    }

    let select_sql = sql.strip_prefix("select ")?.trim();
    if select_sql.starts_with("version()") || select_sql.starts_with("pg_catalog.version()") {
        return Some(one_row(
            &["version"],
            &["PostgreSQL 14.0-compatible rsduck PG wire adapter"],
        ));
    }
    if select_sql.starts_with("current_database()")
        || select_sql.starts_with("pg_catalog.current_database()")
    {
        return Some(one_row(&["current_database"], &["postgres"]));
    }
    if select_sql.starts_with("current_schema()")
        || select_sql.starts_with("pg_catalog.current_schema()")
    {
        return Some(one_row(&["current_schema"], &["main"]));
    }
    if select_sql.starts_with("current_user")
        || select_sql.starts_with("session_user")
        || select_sql.starts_with("current_role")
        || select_sql.starts_with("user")
    {
        return Some(one_row(&["current_user"], &[current_user]));
    }
    if select_sql.starts_with("pg_backend_pid()")
        || select_sql.starts_with("pg_catalog.pg_backend_pid()")
    {
        return Some(one_row(&["pg_backend_pid"], &["1"]));
    }
    if select_sql.starts_with("pg_is_in_recovery()")
        || select_sql.starts_with("pg_catalog.pg_is_in_recovery()")
    {
        return Some(one_row(&["pg_is_in_recovery"], &["f"]));
    }
    if select_sql.starts_with("inet_server_addr()")
        || select_sql.starts_with("pg_catalog.inet_server_addr()")
    {
        return Some(one_row(&["inet_server_addr"], &["127.0.0.1"]));
    }
    if select_sql.starts_with("inet_server_port()")
        || select_sql.starts_with("pg_catalog.inet_server_port()")
    {
        return Some(one_row(&["inet_server_port"], &["15432"]));
    }
    None
}

fn current_setting_result(sql: &str) -> Option<SqlResult> {
    if !sql.starts_with("select ") || sql.contains(" from ") {
        return None;
    }
    if !sql.contains("current_setting(") {
        return None;
    }

    let setting = first_quoted_literal(sql)?;
    let (_, value) = pg_setting(&setting)?;
    Some(one_row(&["current_setting"], &[value]))
}

fn first_quoted_literal(sql: &str) -> Option<String> {
    let start = sql.find('\'')? + 1;
    let rest = &sql[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn pg_database_result(sql: &str) -> Option<SqlResult> {
    if !contains_from_table(sql, "pg_database") {
        return None;
    }

    if sql.contains("count(") {
        return Some(one_row(&["count"], &["1"]));
    }
    if sql.starts_with("select distinct datlastsysoid") || sql.starts_with("select datlastsysoid") {
        return Some(one_row(&["datlastsysoid"], &["0"]));
    }
    if sql.starts_with("select datname from") {
        return Some(one_row(&["datname"], &["postgres"]));
    }
    if sql.contains("databasename")
        || sql.contains("databaseowner")
        || sql.contains("pg_get_userbyid")
    {
        return Some(one_row(
            &[
                "oid",
                "databasename",
                "databaseowner",
                "description",
                "datistemplate",
                "datallowconn",
                "datconnlimit",
                "datlastsysoid",
                "datfrozenxid",
                "dattablespace",
                "encoding",
                "encodingname",
                "datcollate",
                "datctype",
                "datacl",
                "spcname",
            ],
            &[
                "1",
                "postgres",
                "admin",
                "",
                "f",
                "t",
                "-1",
                "0",
                "0",
                "0",
                "6",
                "UTF8",
                "C",
                "C",
                "",
                "pg_default",
            ],
        ));
    }

    Some(one_row(
        &[
            "oid",
            "datname",
            "datdba",
            "encoding",
            "datcollate",
            "datctype",
            "datistemplate",
            "datallowconn",
            "datconnlimit",
            "datlastsysoid",
            "datfrozenxid",
            "datminmxid",
            "dattablespace",
            "datacl",
        ],
        &[
            "1", "postgres", "10", "6", "C", "C", "f", "t", "-1", "0", "0", "0", "0", "",
        ],
    ))
}

fn pg_settings_result(sql: &str) -> Option<SqlResult> {
    contains_from_table(sql, "pg_settings").then(pg_settings_table_result)
}

fn pg_settings_table_result() -> SqlResult {
    SqlResult::Query {
        columns: vec![
            "name".to_string(),
            "setting".to_string(),
            "unit".to_string(),
            "category".to_string(),
            "short_desc".to_string(),
            "extra_desc".to_string(),
            "context".to_string(),
            "vartype".to_string(),
            "source".to_string(),
            "min_val".to_string(),
            "max_val".to_string(),
            "enumvals".to_string(),
            "boot_val".to_string(),
            "reset_val".to_string(),
            "sourcefile".to_string(),
            "sourceline".to_string(),
            "pending_restart".to_string(),
        ],
        rows: pg_settings_rows()
            .into_iter()
            .map(|(name, setting)| {
                vec![
                    name.to_string(),
                    setting.to_string(),
                    String::new(),
                    "Preset Options".to_string(),
                    String::new(),
                    String::new(),
                    "internal".to_string(),
                    "string".to_string(),
                    "default".to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                    setting.to_string(),
                    setting.to_string(),
                    String::new(),
                    String::new(),
                    "f".to_string(),
                ]
            })
            .collect(),
    }
}

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
            "
            SELECT COALESCE((
                SELECT CASE con.contype
                    WHEN 'p' THEN 'PRIMARY KEY (' || COALESCE((
                        SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                        FROM rsduck_catalog.pg_attribute a
                        WHERE a.attrelid = con.conrelid
                          AND a.attisdropped = FALSE
                          AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                    ), '') || ')'
                    WHEN 'u' THEN 'UNIQUE (' || COALESCE((
                        SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                        FROM rsduck_catalog.pg_attribute a
                        WHERE a.attrelid = con.conrelid
                          AND a.attisdropped = FALSE
                          AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                    ), '') || ')'
                    WHEN 'c' THEN 'CHECK (' || con.conbin || ')'
                    WHEN 'f' THEN 'FOREIGN KEY (' || COALESCE((
                        SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)))
                        FROM rsduck_catalog.pg_attribute a
                        WHERE a.attrelid = con.conrelid
                          AND a.attisdropped = FALSE
                          AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                    ), '') || ') REFERENCES ' || COALESCE((
                        SELECT rn.nspname || '.' || rc.relname
                        FROM rsduck_catalog.pg_class rc
                        JOIN rsduck_catalog.pg_namespace rn ON rn.oid = rc.relnamespace
                        WHERE rc.oid = con.confrelid
                    ), 'unknown') || ' (' || COALESCE((
                        SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(con.confkey, ','), CAST(a.attnum AS VARCHAR)))
                        FROM rsduck_catalog.pg_attribute a
                        WHERE a.attrelid = con.confrelid
                          AND a.attisdropped = FALSE
                          AND COALESCE(list_position(string_split(con.confkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
                    ), '') || ')'
                    ELSE con.contype
                END
                FROM rsduck_catalog.pg_constraint con
                WHERE con.oid = {oid}
            ), 'unknown') AS pg_get_constraintdef
            "
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

fn pg_namespace_sql() -> String {
    format!(
        "
    SELECT
        CAST(n.oid AS VARCHAR) AS oid,
        n.nspname,
        CAST(n.nspowner AS VARCHAR) AS nspowner,
        '' AS nspacl,
        COALESCE(d.description, '') AS description,
        n.nspname AS schema_name,
        COALESCE(u.username, 'unknown') AS schema_owner
    FROM rsduck_catalog.pg_namespace n
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = n.nspowner
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = n.oid AND d.classoid = {PG_NAMESPACE_CLASSOID} AND d.objsubid = 0
    WHERE n.nspname NOT IN ('rsduck_catalog', 'rsduck_internal')
    ORDER BY
        CASE WHEN n.nspname = 'main' THEN 0
             WHEN n.nspname IN ('pg_catalog', 'information_schema') THEN 2
             ELSE 1
        END,
        n.nspname
    "
    )
}

fn pg_class_sql() -> String {
    "
    SELECT
        CAST(c.oid AS VARCHAR) AS oid,
        c.relname AS relname,
        CAST(c.relnamespace AS VARCHAR) AS relnamespace,
        CAST(c.reltype AS VARCHAR) AS reltype,
        '0' AS reloftype,
        CAST(c.relowner AS VARCHAR) AS relowner,
        '0' AS relam,
        '0' AS relfilenode,
        '0' AS reltablespace,
        '0' AS relpages,
        CAST(c.reltuples AS VARCHAR) AS reltuples,
        '0' AS relallvisible,
        '0' AS reltoastrelid,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS relhasindex,
        'f' AS relisshared,
        c.relpersistence AS relpersistence,
        c.relkind AS relkind,
        CAST(c.relnatts AS VARCHAR) AS relnatts,
        CAST((SELECT COUNT(*) FROM rsduck_catalog.pg_constraint con WHERE con.conrelid = c.oid AND con.contype = 'c') AS VARCHAR) AS relchecks,
        'f' AS relhasrules,
        'f' AS relhastriggers,
        'f' AS relhassubclass,
        'f' AS relrowsecurity,
        'f' AS relforcerowsecurity,
        't' AS relispopulated,
        'd' AS relreplident,
        CASE WHEN c.relispartition THEN 't' ELSE 'f' END AS relispartition,
        '0' AS relrewrite,
        '0' AS relfrozenxid,
        '0' AS relminmxid,
        '' AS relacl,
        c.reloptions AS reloptions,
        c.relpartbound AS relpartbound,
        n.nspname AS nspname,
        n.nspname AS schemaname,
        c.relname AS tablename,
        c.relname AS table_name,
        COALESCE(u.username, 'unknown') AS tableowner,
        '' AS tablespace,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS hasindexes,
        'f' AS hasrules,
        'f' AS hastriggers,
        'f' AS rowsecurity,
        COALESCE(d.description, '') AS description,
        c.status AS rsduck_status,
        c.error_message AS rsduck_error_message
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = c.oid AND d.objsubid = 0
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

fn pg_attribute_sql() -> String {
    "
    SELECT
        CAST(a.attrelid * 10000 + a.attnum AS VARCHAR) AS oid,
        CAST(a.attrelid AS VARCHAR) AS attrelid,
        a.attname AS attname,
        CAST(a.atttypid AS VARCHAR) AS atttypid,
        '-1' AS attstattarget,
        '-1' AS attlen,
        CAST(a.attnum AS VARCHAR) AS attnum,
        '0' AS attndims,
        '-1' AS attcacheoff,
        CAST(a.atttypmod AS VARCHAR) AS atttypmod,
        'f' AS attbyval,
        'x' AS attstorage,
        'i' AS attalign,
        CASE WHEN a.attnotnull THEN 't' ELSE 'f' END AS attnotnull,
        CASE WHEN a.atthasdef THEN 't' ELSE 'f' END AS atthasdef,
        'f' AS atthasmissing,
        a.attidentity AS attidentity,
        a.attgenerated AS attgenerated,
        CASE WHEN a.attisdropped THEN 't' ELSE 'f' END AS attisdropped,
        't' AS attislocal,
        '0' AS attinhcount,
        '0' AS attcollation,
        '' AS attacl,
        a.attoptions AS attoptions,
        '' AS attfdwoptions,
        '' AS attmissingval,
        n.nspname AS table_schema,
        c.relname AS table_name,
        a.attname AS column_name,
        CAST(a.attnum AS VARCHAR) AS ordinal_position,
        t.rsduck_physical_type AS data_type,
        CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable,
        COALESCE(def.adbin, '') AS column_default,
        COALESCE(d.description, '') AS description
    FROM rsduck_catalog.pg_attribute a
    JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.pg_type t ON t.oid = a.atttypid
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.pg_attrdef def
      ON def.adrelid = a.attrelid AND def.adnum = a.attnum
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = a.attrelid AND d.objsubid = a.attnum
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
      AND a.attisdropped = FALSE
    ORDER BY n.nspname, c.relname, a.attnum
    "
    .to_string()
}

fn pg_type_sql() -> String {
    "
    SELECT
        CAST(oid AS VARCHAR) AS oid,
        typname,
        CAST(typnamespace AS VARCHAR) AS typnamespace,
        CAST(typowner AS VARCHAR) AS typowner,
        CAST(typlen AS VARCHAR) AS typlen,
        CASE WHEN typbyval THEN 't' ELSE 'f' END AS typbyval,
        typtype,
        typcategory,
        CASE WHEN typisdefined THEN 't' ELSE 'f' END AS typisdefined,
        CAST(typrelid AS VARCHAR) AS typrelid,
        CAST(typelem AS VARCHAR) AS typelem,
        CAST(typarray AS VARCHAR) AS typarray,
        rsduck_physical_type
    FROM rsduck_catalog.pg_type
    ORDER BY oid
    "
    .to_string()
}

fn pg_user_sql() -> String {
    "
    SELECT
        u.username AS usename,
        CAST(u.user_id AS VARCHAR) AS usesysid,
        CASE WHEN EXISTS (
            SELECT 1
            FROM rsduck_catalog.rs_user_role ur
            JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id
            WHERE ur.user_id = u.user_id AND r.role_name = 'admin'
        ) THEN 't' ELSE 'f' END AS usecreatedb,
        CASE WHEN EXISTS (
            SELECT 1
            FROM rsduck_catalog.rs_user_role ur
            JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id
            WHERE ur.user_id = u.user_id AND r.role_name = 'admin'
        ) THEN 't' ELSE 'f' END AS usesuper,
        'f' AS userepl,
        '' AS passwd,
        '' AS valuntil,
        '' AS useconfig
    FROM rsduck_catalog.rs_user u
    WHERE u.status = 'active'
    ORDER BY u.username
    "
    .to_string()
}

fn pg_roles_sql() -> String {
    "
    SELECT
        CAST(role_id AS VARCHAR) AS oid,
        role_name AS rolname,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolsuper,
        't' AS rolinherit,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolcreaterole,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolcreatedb,
        'f' AS rolcanlogin,
        'f' AS rolreplication,
        '-1' AS rolconnlimit,
        '' AS rolpassword,
        '' AS rolvaliduntil,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolbypassrls,
        '' AS rolconfig
    FROM rsduck_catalog.rs_role
    ORDER BY role_name
    "
    .to_string()
}

fn pg_index_sql() -> String {
    "
    SELECT
        CAST(indexrelid AS VARCHAR) AS indexrelid,
        CAST(indrelid AS VARCHAR) AS indrelid,
        CAST(indnatts AS VARCHAR) AS indnatts,
        CAST(indnkeyatts AS VARCHAR) AS indnkeyatts,
        CASE WHEN indisunique THEN 't' ELSE 'f' END AS indisunique,
        CASE WHEN indisprimary THEN 't' ELSE 'f' END AS indisprimary,
        CASE WHEN indisvalid THEN 't' ELSE 'f' END AS indisvalid,
        indkey,
        indexprs,
        indpred
    FROM rsduck_catalog.pg_index
    ORDER BY indexrelid
    "
    .to_string()
}

fn pg_constraint_sql() -> String {
    "
    SELECT
        CAST(con.oid AS VARCHAR) AS oid,
        con.conname,
        CAST(con.connamespace AS VARCHAR) AS connamespace,
        con.contype,
        CAST(con.conrelid AS VARCHAR) AS conrelid,
        CAST(con.conindid AS VARCHAR) AS conindid,
        con.conkey,
        CAST(con.confrelid AS VARCHAR) AS confrelid,
        con.confkey,
        CASE WHEN con.convalidated THEN 't' ELSE 'f' END AS convalidated,
        con.conbin,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        CASE con.contype
            WHEN 'p' THEN 'PRIMARY KEY'
            WHEN 'u' THEN 'UNIQUE'
            WHEN 'c' THEN 'CHECK'
            WHEN 'f' THEN 'FOREIGN KEY'
            ELSE con.contype
        END AS constraint_type
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    ORDER BY n.nspname, con.conname
    "
    .to_string()
}

fn information_schema_key_column_usage_sql() -> String {
    "
    SELECT
        'postgres' AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        'postgres' AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        CAST(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)) AS VARCHAR) AS ordinal_position,
        '' AS position_in_unique_constraint
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    JOIN rsduck_catalog.pg_attribute a
      ON a.attrelid = con.conrelid
     AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
    WHERE con.contype IN ('p', 'u', 'f')
      AND con.conkey <> ''
    ORDER BY tn.nspname, tc.relname, con.conname,
             list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR))
    "
    .to_string()
}

fn information_schema_constraint_column_usage_sql() -> String {
    "
    SELECT
        'postgres' AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        'postgres' AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    JOIN rsduck_catalog.pg_attribute a
      ON a.attrelid = con.conrelid
     AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
    WHERE con.conkey <> ''
    ORDER BY tn.nspname, tc.relname, con.conname,
             list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR))
    "
    .to_string()
}

fn pg_attrdef_sql() -> String {
    "
    SELECT
        CAST(oid AS VARCHAR) AS oid,
        CAST(adrelid AS VARCHAR) AS adrelid,
        CAST(adnum AS VARCHAR) AS adnum,
        adbin
    FROM rsduck_catalog.pg_attrdef
    ORDER BY adrelid, adnum
    "
    .to_string()
}

fn pg_depend_sql() -> String {
    "
    SELECT
        CAST(classid AS VARCHAR) AS classid,
        CAST(objid AS VARCHAR) AS objid,
        CAST(objsubid AS VARCHAR) AS objsubid,
        CAST(refclassid AS VARCHAR) AS refclassid,
        CAST(refobjid AS VARCHAR) AS refobjid,
        CAST(refobjsubid AS VARCHAR) AS refobjsubid,
        deptype
    FROM rsduck_catalog.pg_depend
    ORDER BY classid, objid, refclassid, refobjid
    "
    .to_string()
}

fn pg_description_sql() -> String {
    "
    SELECT
        CAST(objoid AS VARCHAR) AS objoid,
        CAST(classoid AS VARCHAR) AS classoid,
        CAST(objsubid AS VARCHAR) AS objsubid,
        description
    FROM rsduck_catalog.pg_description
    ORDER BY objoid, objsubid
    "
    .to_string()
}

fn pg_views_sql() -> String {
    "
    SELECT
        n.nspname AS schemaname,
        c.relname AS viewname,
        COALESCE(u.username, 'unknown') AS viewowner,
        ext.generated_sql AS definition
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    WHERE c.status = 'active'
      AND c.relkind = 'v'
      AND ext.visibility = 'user'
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

fn pg_indexes_sql() -> String {
    "
    SELECT
        tn.nspname AS schemaname,
        tc.relname AS tablename,
        inx.relname AS indexname,
        '' AS tablespace,
        'CREATE INDEX ' || inx.relname || ' ON ' || tn.nspname || '.' || tc.relname AS indexdef
    FROM rsduck_catalog.pg_index i
    JOIN rsduck_catalog.pg_class inx ON inx.oid = i.indexrelid
    JOIN rsduck_catalog.pg_class tc ON tc.oid = i.indrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    ORDER BY tn.nspname, tc.relname, inx.relname
    "
    .to_string()
}

fn empty_pg_catalog_sql(sql: &str) -> Option<String> {
    if contains_from_table(sql, "pg_trigger") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '0' AS tgrelid,
                '' AS tgname,
                '0' AS tgfoid,
                '0' AS tgtype,
                '' AS tgenabled,
                'f' AS tgisinternal,
                '0' AS tgconstrrelid,
                '0' AS tgconstrindid,
                '0' AS tgconstraint,
                'f' AS tgdeferrable,
                'f' AS tginitdeferred,
                '0' AS tgnargs,
                '' AS tgattr,
                '' AS tgargs,
                '' AS tgqual,
                '' AS tgoldtable,
                '' AS tgnewtable
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_proc") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS proname,
                '0' AS pronamespace,
                '0' AS proowner,
                '0' AS prolang,
                '0' AS procost,
                '0' AS prorows,
                '0' AS provariadic,
                '-' AS prosupport,
                'f' AS prokind,
                'f' AS prosecdef,
                'f' AS proleakproof,
                'f' AS proisstrict,
                'f' AS proretset,
                'v' AS provolatile,
                'u' AS proparallel,
                '0' AS pronargs,
                '0' AS pronargdefaults,
                '0' AS prorettype,
                '' AS proargtypes,
                '' AS proallargtypes,
                '' AS proargmodes,
                '' AS proargnames,
                '' AS proargdefaults,
                '' AS protrftypes,
                '' AS prosrc,
                '' AS probin,
                '' AS prosqlbody,
                '' AS proconfig,
                '' AS proacl
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_extension") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS extname,
                '0' AS extowner,
                '0' AS extnamespace,
                'f' AS extrelocatable,
                '' AS extversion,
                '' AS extconfig,
                '' AS extcondition
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_policy") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS polname,
                '0' AS polrelid,
                '' AS polcmd,
                'f' AS polpermissive,
                '' AS polroles,
                '' AS polqual,
                '' AS polwithcheck
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_matviews") {
        return Some(
            "
            SELECT
                '' AS schemaname,
                '' AS matviewname,
                '' AS matviewowner,
                '' AS tablespace,
                'f' AS hasindexes,
                'f' AS ispopulated,
                '' AS definition
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_sequences") {
        return Some(
            "
            SELECT
                '' AS schemaname,
                '' AS sequencename,
                '' AS sequenceowner,
                '' AS data_type,
                '' AS start_value,
                '' AS min_value,
                '' AS max_value,
                '' AS increment_by,
                'f' AS cycle,
                '' AS cache_size,
                '' AS last_value
            WHERE FALSE
            "
            .to_string(),
        );
    }
    None
}

fn contains_from_table(sql: &str, table: &str) -> bool {
    let pg_catalog_table = format!("pg_catalog.{table}");
    let quoted_pg_catalog_table = format!("\"pg_catalog\".\"{table}\"");
    sql.contains(&format!(" from {table}"))
        || sql.contains(&format!(" from {pg_catalog_table}"))
        || sql.contains(&format!(" from {quoted_pg_catalog_table}"))
        || sql.contains(&format!(" join {table}"))
        || sql.contains(&format!(" join {pg_catalog_table}"))
        || sql.contains(&format!(" join {quoted_pg_catalog_table}"))
}

fn pg_setting(name: &str) -> Option<(&'static str, &'static str)> {
    match name.trim_matches('"') {
        "application_name" => Some(("application_name", "rsduck")),
        "client_encoding" => Some(("client_encoding", "UTF8")),
        "datestyle" => Some(("DateStyle", "ISO, MDY")),
        "default_transaction_read_only" => Some(("default_transaction_read_only", "off")),
        "extra_float_digits" => Some(("extra_float_digits", "3")),
        "integer_datetimes" => Some(("integer_datetimes", "on")),
        "is_superuser" => Some(("is_superuser", "on")),
        "lc_collate" => Some(("lc_collate", "C")),
        "lc_ctype" => Some(("lc_ctype", "C")),
        "max_identifier_length" => Some(("max_identifier_length", "63")),
        "search_path" => Some(("search_path", "main")),
        "server_encoding" => Some(("server_encoding", "UTF8")),
        "server_version" => Some(("server_version", "14.0")),
        "server_version_num" => Some(("server_version_num", "140000")),
        "standard_conforming_strings" => Some(("standard_conforming_strings", "on")),
        "timezone" => Some(("TimeZone", "UTC")),
        "transaction_isolation" => Some(("transaction_isolation", "read committed")),
        "transaction_read_only" => Some(("transaction_read_only", "off")),
        _ => None,
    }
}

fn pg_settings_rows() -> Vec<(&'static str, &'static str)> {
    vec![
        ("application_name", "rsduck"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("default_transaction_read_only", "off"),
        ("extra_float_digits", "3"),
        ("integer_datetimes", "on"),
        ("is_superuser", "on"),
        ("lc_collate", "C"),
        ("lc_ctype", "C"),
        ("max_identifier_length", "63"),
        ("search_path", "main"),
        ("server_encoding", "UTF8"),
        ("server_version", "14.0"),
        ("server_version_num", "140000"),
        ("standard_conforming_strings", "on"),
        ("TimeZone", "UTC"),
        ("transaction_isolation", "read committed"),
        ("transaction_read_only", "off"),
    ]
}

#[cfg(test)]
mod tests {
    use super::{compat_result, rewrite_sql};
    use crate::db::SqlResult;
    use crate::sql_route::{route_sql, SqlRoute};
    use duckdb::Connection;

    #[test]
    fn navicat_datlastsysoid_probe_returns_pg_compat_row() {
        let result = compat_result("SELECT DISTINCT datlastsysoid FROM pg_database;", "admin")
            .expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["datlastsysoid"]);
                assert_eq!(rows, vec![vec!["0"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn regular_sql_is_not_intercepted() {
        assert!(compat_result("SELECT * FROM kline_day", "admin").is_none());
    }

    #[test]
    fn defined_empty_pg_catalog_relations_rewrite_to_empty_results() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        for relation in [
            "pg_trigger",
            "pg_proc",
            "pg_extension",
            "pg_policy",
            "pg_matviews",
            "pg_sequences",
        ] {
            let sql = rewrite_sql(&format!("SELECT * FROM pg_catalog.{relation}"))
                .expect("rewrite empty pg_catalog relation");
            assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM ({sql})"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{relation}");
        }
    }

    #[test]
    fn navicat_datestyle_probe_returns_pg_compat_row() {
        let result = compat_result("SHOW DateStyle;", "admin").expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["DateStyle"]);
                assert_eq!(rows, vec![vec!["ISO, MDY"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn navicat_database_list_probe_returns_pg_compat_row() {
        let result = compat_result(
            "
            SELECT oid, datname AS databasename, pg_get_userbyid(datdba) AS databaseowner,
                   des.description AS description
            FROM pg_database d
            LEFT JOIN pg_shdescription des ON des.objoid = d.oid
            ",
            "admin",
        )
        .expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(&columns[0..3], ["oid", "databasename", "databaseowner"]);
                assert_eq!(rows[0][1], "postgres");
                assert_eq!(rows[0][2], "admin");
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn pg_session_setting_probes_are_accepted() {
        let show_result = compat_result("SHOW server_version;", "admin").expect("compat result");
        match show_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["server_version"]);
                assert_eq!(rows, vec![vec!["14.0"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let setting_result =
            compat_result("SELECT current_setting('server_version_num');", "admin")
                .expect("compat result");
        match setting_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_setting"]);
                assert_eq!(rows, vec![vec!["140000"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let set_result =
            compat_result("SET extra_float_digits = 3;", "admin").expect("compat result");
        match set_result {
            SqlResult::Execute {
                command,
                affected_rows,
            } => {
                assert_eq!(command, "SET");
                assert_eq!(affected_rows, 0);
            }
            SqlResult::Query { .. } => panic!("expected execute result"),
        }
    }

    #[test]
    fn current_user_uses_authenticated_session_user() {
        let result = compat_result("SELECT current_user;", "alice").expect("compat result");
        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_user"]);
                assert_eq!(rows, vec![vec!["alice"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn pg_user_and_roles_rewrite_from_catalog_accounts() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let user_sql = rewrite_sql("SELECT usename FROM pg_catalog.pg_user").expect("pg_user");
        assert_eq!(route_sql(&user_sql).unwrap().route, SqlRoute::Read);
        let alice_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({user_sql}) WHERE usename = 'alice'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alice_count, 1);

        let roles_sql = rewrite_sql("SELECT rolname FROM pg_catalog.pg_roles").expect("pg_roles");
        assert_eq!(route_sql(&roles_sql).unwrap().route, SqlRoute::Read);
        let role_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({roles_sql}) WHERE rolname IN ('admin', 'reader')"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role_count, 2);
    }

    #[test]
    fn owner_projection_uses_catalog_user_names() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER owner_user PASSWORD='pw'")
            .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE SCHEMA owned_schema",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON SCHEMA owned_schema IS 'owned schema comment'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE TABLE owned_schema.owned_table(id INTEGER)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE VIEW owned_schema.owned_view AS SELECT * FROM owned_schema.owned_table",
        )
        .unwrap();

        let schema_sql = rewrite_sql(
            "SELECT schema_owner, description FROM information_schema.schemata WHERE schema_name = 'owned_schema'",
        )
        .expect("rewrite schema owner");
        let (schema_owner, schema_description): (String, String) = conn
            .query_row(&schema_sql, [], |row| {
                Ok((row.get("schema_owner")?, row.get("description")?))
            })
            .unwrap();
        assert_eq!(schema_owner, "owner_user");
        assert_eq!(schema_description, "owned schema comment");

        let table_sql = rewrite_sql(
            "SELECT tableowner FROM pg_catalog.pg_tables WHERE tablename = 'owned_table'",
        )
        .expect("rewrite table owner");
        let table_owner: String = conn
            .query_row(&table_sql, [], |row| row.get("tableowner"))
            .unwrap();
        assert_eq!(table_owner, "owner_user");

        let view_sql =
            rewrite_sql("SELECT viewowner FROM pg_catalog.pg_views WHERE viewname = 'owned_view'")
                .expect("rewrite view owner");
        let view_owner: String = conn
            .query_row(&view_sql, [], |row| row.get("viewowner"))
            .unwrap();
        assert_eq!(view_owner, "owner_user");
    }

    #[test]
    fn pg_class_rewrite_returns_duckdb_tables() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT c.oid, c.relname, n.nspname
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE c.relkind IN ('r', 'p')
            ",
        )
        .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let table_name: String = conn.query_row(&sql, [], |row| row.get("relname")).unwrap();
        assert_eq!(table_name, "kline_day");
    }

    #[test]
    fn pg_class_rewrite_returns_partitioned_table_relkind() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();

        let sql =
            rewrite_sql("SELECT relname, relkind FROM pg_catalog.pg_class").expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let relkind: String = conn
            .query_row(
                &format!("SELECT relkind FROM ({sql}) WHERE relname = 'ods_access_log'"),
                [],
                |row| row.get("relkind"),
            )
            .unwrap();
        assert_eq!(relkind, "p");
    }

    #[test]
    fn pg_attribute_rewrite_returns_duckdb_columns() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT a.attname
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            WHERE c.relname = 'kline_day'
            ORDER BY CAST(a.attnum AS INTEGER)
            ",
        )
        .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let column_name: String = conn.query_row(&sql, [], |row| row.get("attname")).unwrap();
        assert_eq!(column_name, "code");
    }

    #[test]
    fn pg_index_rewrite_returns_catalog_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE INDEX idx_kline_day_code ON kline_day(code)",
        )
        .unwrap();

        let sql = rewrite_sql("SELECT indexrelid, indrelid FROM pg_catalog.pg_index")
            .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let indexrelid: String = conn
            .query_row(&sql, [], |row| row.get("indexrelid"))
            .unwrap();
        assert!(!indexrelid.is_empty());
    }

    #[test]
    fn information_schema_constraint_columns_rewrite_from_catalog_constraints() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(
                code VARCHAR NOT NULL,
                bar_time TIMESTAMP NOT NULL,
                close DOUBLE,
                PRIMARY KEY(code, bar_time),
                UNIQUE(close)
            )",
        )
        .unwrap();

        let key_sql = rewrite_sql(
            "SELECT constraint_name, column_name, ordinal_position
             FROM information_schema.key_column_usage",
        )
        .expect("rewrite key column usage");
        assert_eq!(route_sql(&key_sql).unwrap().route, SqlRoute::Read);
        let first_key_column: String = conn
            .query_row(
                &format!(
                    "SELECT column_name FROM ({key_sql}) \
                     WHERE constraint_name = 'kline_day_pkey' \
                     ORDER BY CAST(ordinal_position AS INTEGER)"
                ),
                [],
                |row| row.get("column_name"),
            )
            .unwrap();
        assert_eq!(first_key_column, "code");

        let usage_sql = rewrite_sql(
            "SELECT table_name, column_name
             FROM information_schema.constraint_column_usage",
        )
        .expect("rewrite constraint column usage");
        assert_eq!(route_sql(&usage_sql).unwrap().route, SqlRoute::Read);
        let close_usage_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM ({usage_sql}) \
                     WHERE table_name = 'kline_day' AND column_name = 'close'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(close_usage_count, 1);
    }

    #[test]
    fn pg_catalog_scalar_functions_rewrite_to_catalog_queries() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE quotes(
                code VARCHAR,
                close DOUBLE DEFAULT 0,
                PRIMARY KEY(code)
            )",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON TABLE quotes IS 'quotes table'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON COLUMN quotes.close IS 'close price'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE quote_ticks(
                code VARCHAR,
                CONSTRAINT quote_ticks_code_fkey FOREIGN KEY(code) REFERENCES quotes(code)
            )",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let type_sql = rewrite_sql("SELECT format_type(1043, -1)").expect("format_type rewrite");
        let type_name: String = conn
            .query_row(&type_sql, [], |row| row.get("format_type"))
            .unwrap();
        assert_eq!(type_name, "varchar");

        let expr_sql =
            rewrite_sql("SELECT pg_get_expr('close + 1', 0)").expect("pg_get_expr rewrite");
        let expr: String = conn
            .query_row(&expr_sql, [], |row| row.get("pg_get_expr"))
            .unwrap();
        assert_eq!(expr, "close + 1");

        let constraint_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_constraint WHERE conname = 'quotes_pkey'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let constraint_sql = rewrite_sql(&format!("SELECT pg_get_constraintdef({constraint_oid})"))
            .expect("pg_get_constraintdef rewrite");
        let constraint_def: String = conn
            .query_row(&constraint_sql, [], |row| row.get("pg_get_constraintdef"))
            .unwrap();
        assert_eq!(constraint_def, "PRIMARY KEY (code)");
        let fk_constraint_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_constraint WHERE conname = 'quote_ticks_code_fkey'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fk_constraint_sql =
            rewrite_sql(&format!("SELECT pg_get_constraintdef({fk_constraint_oid})"))
                .expect("foreign key constraintdef rewrite");
        let fk_constraint_def: String = conn
            .query_row(&fk_constraint_sql, [], |row| {
                row.get("pg_get_constraintdef")
            })
            .unwrap();
        assert_eq!(
            fk_constraint_def,
            "FOREIGN KEY (code) REFERENCES main.quotes (code)"
        );

        let table_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE SCHEMA archive").unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE archive.hidden_quotes(code VARCHAR)",
        )
        .unwrap();
        let hidden_table_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'hidden_quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let visible_sql = rewrite_sql(&format!("SELECT pg_table_is_visible({table_oid})"))
            .expect("pg_table_is_visible main relation");
        let visible: String = conn
            .query_row(&visible_sql, [], |row| row.get("pg_table_is_visible"))
            .unwrap();
        assert_eq!(visible, "t");
        let hidden_visible_sql =
            rewrite_sql(&format!("SELECT pg_table_is_visible({hidden_table_oid})"))
                .expect("pg_table_is_visible non-main relation");
        let hidden_visible: String = conn
            .query_row(&hidden_visible_sql, [], |row| {
                row.get("pg_table_is_visible")
            })
            .unwrap();
        assert_eq!(hidden_visible, "f");

        let table_desc_sql =
            rewrite_sql(&format!("SELECT obj_description({table_oid})")).expect("description");
        let table_desc: String = conn
            .query_row(&table_desc_sql, [], |row| row.get("obj_description"))
            .unwrap();
        assert_eq!(table_desc, "quotes table");

        let close_attnum: i64 = conn
            .query_row(
                "SELECT a.attnum \
                 FROM rsduck_catalog.pg_attribute a \
                 JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
                 WHERE c.relname = 'quotes' AND a.attname = 'close'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let column_desc_sql = rewrite_sql(&format!(
            "SELECT col_description({table_oid}, {close_attnum})"
        ))
        .expect("column description");
        let column_desc: String = conn
            .query_row(&column_desc_sql, [], |row| row.get("col_description"))
            .unwrap();
        assert_eq!(column_desc, "close price");

        let alice_id: i64 = conn
            .query_row(
                "SELECT user_id FROM rsduck_catalog.rs_user WHERE username = 'alice'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let user_sql =
            rewrite_sql(&format!("SELECT pg_get_userbyid({alice_id})")).expect("user lookup");
        let username: String = conn
            .query_row(&user_sql, [], |row| row.get("pg_get_userbyid"))
            .unwrap();
        assert_eq!(username, "alice");
    }

    #[test]
    fn pg_namespace_rewrite_returns_duckdb_schemas() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        let sql =
            rewrite_sql("SELECT oid, nspname FROM pg_catalog.pg_namespace").expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let schema_name: String = conn.query_row(&sql, [], |row| row.get("nspname")).unwrap();
        assert_eq!(schema_name, "main");
    }
}

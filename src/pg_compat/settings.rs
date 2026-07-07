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

fn pg_database_legacy_result(sql: &str) -> Option<SqlResult> {
    if !contains_from_table(sql, "pg_database") {
        return None;
    }

    if !sql.contains("pg_shdescription") && !sql.contains("pg_get_userbyid") {
        return None;
    }

    Some(one_row(
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
    ))
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

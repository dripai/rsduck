use crate::db::SqlResult;

pub fn compat_result(sql: &str) -> Option<SqlResult> {
    let normalized = normalize_sql(sql);
    pg_set_result(&normalized)
        .or_else(|| pg_show_result(&normalized))
        .or_else(|| pg_scalar_result(&normalized))
        .or_else(|| pg_database_result(&normalized))
        .or_else(|| pg_user_result(&normalized))
        .or_else(|| pg_settings_result(&normalized))
}

pub fn rewrite_sql(sql: &str) -> Option<String> {
    let normalized = normalize_sql(sql);
    if !normalized.starts_with("select ") && !normalized.starts_with("with ") {
        return None;
    }

    if contains_from_table(&normalized, "pg_attribute")
        || contains_relation(&normalized, "information_schema.columns")
    {
        return Some(pg_attribute_sql());
    }
    if contains_from_table(&normalized, "pg_class")
        || contains_from_table(&normalized, "pg_tables")
        || contains_relation(&normalized, "information_schema.tables")
    {
        return Some(pg_class_sql());
    }
    if contains_from_table(&normalized, "pg_namespace")
        || contains_relation(&normalized, "information_schema.schemata")
    {
        return Some(pg_namespace_sql());
    }
    if contains_from_table(&normalized, "pg_type") {
        return Some(pg_type_sql());
    }

    None
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

fn pg_scalar_result(sql: &str) -> Option<SqlResult> {
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
        return Some(one_row(&["current_user"], &["postgres"]));
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
    if select_sql.starts_with("pg_get_userbyid(")
        || select_sql.starts_with("pg_catalog.pg_get_userbyid(")
    {
        return Some(one_row(&["pg_get_userbyid"], &["postgres"]));
    }
    if select_sql.starts_with("has_database_privilege(")
        || select_sql.starts_with("has_schema_privilege(")
        || select_sql.starts_with("has_table_privilege(")
        || select_sql.starts_with("pg_catalog.has_database_privilege(")
        || select_sql.starts_with("pg_catalog.has_schema_privilege(")
        || select_sql.starts_with("pg_catalog.has_table_privilege(")
    {
        return Some(one_row(&["has_privilege"], &["t"]));
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
                "postgres",
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

fn pg_user_result(sql: &str) -> Option<SqlResult> {
    if contains_from_table(sql, "pg_user") {
        return Some(one_row(
            &[
                "usename",
                "usesysid",
                "usecreatedb",
                "usesuper",
                "userepl",
                "passwd",
                "valuntil",
                "useconfig",
            ],
            &["postgres", "10", "t", "t", "t", "", "", ""],
        ));
    }

    if contains_from_table(sql, "pg_roles") || contains_from_table(sql, "pg_authid") {
        return Some(one_row(
            &[
                "oid",
                "rolname",
                "rolsuper",
                "rolinherit",
                "rolcreaterole",
                "rolcreatedb",
                "rolcanlogin",
                "rolreplication",
                "rolconnlimit",
                "rolpassword",
                "rolvaliduntil",
                "rolbypassrls",
                "rolconfig",
            ],
            &[
                "10", "postgres", "t", "t", "t", "t", "t", "t", "-1", "", "", "t", "",
            ],
        ));
    }

    None
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

fn pg_namespace_sql() -> String {
    "
    SELECT DISTINCT
        CAST(database_oid AS VARCHAR) AS oid,
        schema_name AS nspname,
        '10' AS nspowner,
        '' AS nspacl,
        '' AS description,
        schema_name AS schema_name,
        schema_name AS schema_owner
    FROM duckdb_schemas()
    WHERE schema_name NOT IN ('information_schema', 'pg_catalog')
    ORDER BY schema_name
    "
    .to_string()
}

fn pg_class_sql() -> String {
    "
    SELECT
        CAST(table_oid AS VARCHAR) AS oid,
        table_name AS relname,
        CAST(schema_oid AS VARCHAR) AS relnamespace,
        '0' AS reltype,
        '0' AS reloftype,
        '10' AS relowner,
        '0' AS relam,
        '0' AS relfilenode,
        '0' AS reltablespace,
        '0' AS relpages,
        CAST(estimated_size AS VARCHAR) AS reltuples,
        '0' AS relallvisible,
        '0' AS reltoastrelid,
        CASE WHEN index_count > 0 THEN 't' ELSE 'f' END AS relhasindex,
        'f' AS relisshared,
        CASE WHEN temporary THEN 't' ELSE 'p' END AS relpersistence,
        'r' AS relkind,
        CAST(column_count AS VARCHAR) AS relnatts,
        CAST(check_constraint_count AS VARCHAR) AS relchecks,
        'f' AS relhasrules,
        'f' AS relhastriggers,
        'f' AS relhassubclass,
        'f' AS relrowsecurity,
        'f' AS relforcerowsecurity,
        't' AS relispopulated,
        'd' AS relreplident,
        'f' AS relispartition,
        '0' AS relrewrite,
        '0' AS relfrozenxid,
        '0' AS relminmxid,
        '' AS relacl,
        '' AS reloptions,
        '' AS relpartbound,
        schema_name AS nspname,
        schema_name AS schemaname,
        table_name AS tablename,
        table_name AS table_name,
        'postgres' AS tableowner,
        '' AS tablespace,
        CASE WHEN index_count > 0 THEN 't' ELSE 'f' END AS hasindexes,
        'f' AS hasrules,
        'f' AS hastriggers,
        'f' AS rowsecurity,
        '' AS description
    FROM duckdb_tables()
    WHERE internal = false
    ORDER BY schema_name, table_name
    "
    .to_string()
}

fn pg_attribute_sql() -> String {
    "
    SELECT
        CAST(table_oid * 10000 + column_index AS VARCHAR) AS oid,
        CAST(table_oid AS VARCHAR) AS attrelid,
        column_name AS attname,
        CASE
            WHEN lower(data_type) LIKE '%int%' THEN '23'
            WHEN lower(data_type) IN ('float', 'real') THEN '700'
            WHEN lower(data_type) IN ('double', 'double precision', 'decimal', 'numeric') THEN '701'
            WHEN lower(data_type) IN ('boolean', 'bool') THEN '16'
            WHEN lower(data_type) LIKE '%timestamp%' THEN '1114'
            WHEN lower(data_type) = 'date' THEN '1082'
            WHEN lower(data_type) = 'time' THEN '1083'
            ELSE '25'
        END AS atttypid,
        '-1' AS attstattarget,
        '-1' AS attlen,
        CAST(column_index AS VARCHAR) AS attnum,
        '0' AS attndims,
        '-1' AS attcacheoff,
        '-1' AS atttypmod,
        'f' AS attbyval,
        'x' AS attstorage,
        'i' AS attalign,
        CASE WHEN is_nullable THEN 'f' ELSE 't' END AS attnotnull,
        CASE WHEN column_default IS NULL THEN 'f' ELSE 't' END AS atthasdef,
        'f' AS atthasmissing,
        '' AS attidentity,
        '' AS attgenerated,
        'f' AS attisdropped,
        't' AS attislocal,
        '0' AS attinhcount,
        '0' AS attcollation,
        '' AS attacl,
        '' AS attoptions,
        '' AS attfdwoptions,
        '' AS attmissingval,
        schema_name AS table_schema,
        table_name AS table_name,
        column_name AS column_name,
        CAST(column_index AS VARCHAR) AS ordinal_position,
        data_type AS data_type,
        CASE WHEN is_nullable THEN 'YES' ELSE 'NO' END AS is_nullable,
        COALESCE(column_default, '') AS column_default,
        '' AS description
    FROM duckdb_columns()
    WHERE internal = false
    ORDER BY schema_name, table_name, column_index
    "
    .to_string()
}

fn pg_type_sql() -> String {
    "
    SELECT *
    FROM (
        VALUES
            ('16', 'bool', 'b', 't'),
            ('20', 'int8', 'b', 't'),
            ('21', 'int2', 'b', 't'),
            ('23', 'int4', 'b', 't'),
            ('25', 'text', 'b', 't'),
            ('700', 'float4', 'b', 't'),
            ('701', 'float8', 'b', 't'),
            ('1043', 'varchar', 'b', 't'),
            ('1082', 'date', 'b', 't'),
            ('1083', 'time', 'b', 't'),
            ('1114', 'timestamp', 'b', 't'),
            ('1700', 'numeric', 'b', 't')
    ) AS t(oid, typname, typtype, typisdefined)
    "
    .to_string()
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

fn contains_relation(sql: &str, relation: &str) -> bool {
    let quoted_relation = relation
        .split('.')
        .map(|part| format!("\"{part}\""))
        .collect::<Vec<_>>()
        .join(".");
    sql.contains(&format!(" from {relation}"))
        || sql.contains(&format!(" join {relation}"))
        || sql.contains(&format!(" from {quoted_relation}"))
        || sql.contains(&format!(" join {quoted_relation}"))
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
    use duckdb::Connection;

    #[test]
    fn navicat_datlastsysoid_probe_returns_pg_compat_row() {
        let result = compat_result("SELECT DISTINCT datlastsysoid FROM pg_database;")
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
        assert!(compat_result("SELECT * FROM kline_day").is_none());
    }

    #[test]
    fn navicat_datestyle_probe_returns_pg_compat_row() {
        let result = compat_result("SHOW DateStyle;").expect("compat result");

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
        )
        .expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(&columns[0..3], ["oid", "databasename", "databaseowner"]);
                assert_eq!(rows[0][1], "postgres");
                assert_eq!(rows[0][2], "postgres");
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn pg_session_setting_probes_are_accepted() {
        let show_result = compat_result("SHOW server_version;").expect("compat result");
        match show_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["server_version"]);
                assert_eq!(rows, vec![vec!["14.0"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let setting_result =
            compat_result("SELECT current_setting('server_version_num');").expect("compat result");
        match setting_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_setting"]);
                assert_eq!(rows, vec![vec!["140000"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let set_result = compat_result("SET extra_float_digits = 3;").expect("compat result");
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
    fn pg_class_rewrite_returns_duckdb_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE kline_day(code VARCHAR, close DOUBLE);")
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

        let table_name: String = conn
            .query_row(&sql, [], |row| row.get("table_name"))
            .unwrap();
        assert_eq!(table_name, "kline_day");
    }

    #[test]
    fn pg_attribute_rewrite_returns_duckdb_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE kline_day(code VARCHAR, close DOUBLE);")
            .unwrap();

        let sql = rewrite_sql(
            "
            SELECT a.attname
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            ",
        )
        .expect("rewrite sql");

        let column_name: String = conn
            .query_row(&sql, [], |row| row.get("column_name"))
            .unwrap();
        assert_eq!(column_name, "code");
    }

    #[test]
    fn pg_namespace_rewrite_returns_duckdb_schemas() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE kline_day(code VARCHAR);")
            .unwrap();
        let sql =
            rewrite_sql("SELECT oid, nspname FROM pg_catalog.pg_namespace").expect("rewrite sql");

        let schema_name: String = conn.query_row(&sql, [], |row| row.get("nspname")).unwrap();
        assert_eq!(schema_name, "main");
    }
}

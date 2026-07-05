use crate::db::SqlResult;

pub fn compat_result(sql: &str, current_user: &str) -> Option<SqlResult> {
    let normalized = normalize_sql(sql);
    pg_set_result(&normalized)
        .or_else(|| pg_show_result(&normalized))
        .or_else(|| pg_scalar_result(&normalized, current_user))
        .or_else(|| pg_database_result(&normalized))
        .or_else(|| pg_user_result(&normalized, current_user))
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
    if contains_from_table(&normalized, "pg_index") {
        return Some(pg_index_sql());
    }
    if contains_from_table(&normalized, "pg_constraint")
        || contains_relation(&normalized, "information_schema.table_constraints")
    {
        return Some(pg_constraint_sql());
    }
    if contains_from_table(&normalized, "pg_attrdef") {
        return Some(pg_attrdef_sql());
    }
    if contains_from_table(&normalized, "pg_depend") {
        return Some(pg_depend_sql());
    }
    if contains_from_table(&normalized, "pg_description") {
        return Some(pg_description_sql());
    }
    if contains_from_table(&normalized, "pg_views")
        || contains_relation(&normalized, "information_schema.views")
    {
        return Some(pg_views_sql());
    }
    if contains_from_table(&normalized, "pg_indexes") {
        return Some(pg_indexes_sql());
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
    if select_sql.starts_with("pg_get_userbyid(")
        || select_sql.starts_with("pg_catalog.pg_get_userbyid(")
    {
        return Some(one_row(&["pg_get_userbyid"], &[current_user]));
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

fn pg_user_result(sql: &str, current_user: &str) -> Option<SqlResult> {
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
            &[current_user, "10", "t", "t", "f", "", "", ""],
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
                "10",
                current_user,
                "t",
                "t",
                "t",
                "t",
                "t",
                "f",
                "-1",
                "",
                "",
                "t",
                "",
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
    SELECT
        CAST(oid AS VARCHAR) AS oid,
        nspname,
        CAST(nspowner AS VARCHAR) AS nspowner,
        '' AS nspacl,
        '' AS description,
        nspname AS schema_name,
        'admin' AS schema_owner
    FROM rsduck_catalog.pg_namespace
    WHERE nspname NOT IN ('rsduck_catalog', 'rsduck_internal')
    ORDER BY
        CASE WHEN nspname = 'main' THEN 0
             WHEN nspname IN ('pg_catalog', 'information_schema') THEN 2
             ELSE 1
        END,
        nspname
    "
    .to_string()
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
        'admin' AS tableowner,
        '' AS tablespace,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS hasindexes,
        'f' AS hasrules,
        'f' AS hastriggers,
        'f' AS rowsecurity,
        COALESCE(d.description, '') AS description
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = c.oid AND d.objsubid = 0
    WHERE c.status = 'active'
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
    WHERE c.status = 'active'
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
        'admin' AS viewowner,
        ext.generated_sql AS definition
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
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

        let table_name: String = conn
            .query_row(&sql, [], |row| row.get("table_name"))
            .unwrap();
        assert_eq!(table_name, "kline_day");
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
            ",
        )
        .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let column_name: String = conn
            .query_row(&sql, [], |row| row.get("column_name"))
            .unwrap();
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

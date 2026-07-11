use crate::db::{SqlColumn, SqlType, SqlTypedResult, SqlValue};
use duckdb::Connection;

pub fn compat_result(sql: &str) -> Option<SqlTypedResult> {
    let normalized = sql.trim().trim_end_matches(';').trim().to_ascii_lowercase();
    if references_information_schema_engines(sql) || normalized == "show engines" {
        return Some(mysql_engines_result());
    }
    None
}

pub fn rewrite_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    show_table_status_sql(sql, current_schema, username)
        .or_else(|| show_table_detail_sql(sql, current_schema, username))
        .or_else(|| show_tables_sql(sql, current_schema, username))
        .or_else(|| show_columns_sql(sql, current_schema, username))
        .or_else(|| show_index_sql(sql, current_schema, username))
        .or_else(|| show_routine_status_sql(sql, current_schema))
        .or_else(|| rewrite_mysql_system_sql(sql))
        .or_else(|| rewrite_information_schema_sql(sql, username))
}

pub fn is_show_table_detail(sql: &str) -> bool {
    parse_show_table_status_query(sql).is_none() && parse_show_table_detail_query(sql).is_some()
}

pub fn is_mysql_system_projection(sql: &str) -> bool {
    let compact = sql
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '`' && *ch != '"')
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "user",
        "role_edges",
        "default_roles",
        "db",
        "procs_priv",
        "tables_priv",
        "columns_priv",
    ]
    .iter()
    .any(|relation| compact.contains(&format!("mysql.{relation}")))
}

pub fn validate_metadata_projection(conn: &Connection, username: &str) -> Result<(), String> {
    let visibility = visible_relation_predicate(username, "c", "n");
    let username = sql_literal(username);
    let sql = format!(
        "
        WITH physical_relations AS (
            SELECT schema_name, table_name, 'table' AS kind
            FROM duckdb_tables()
            WHERE internal = FALSE
            UNION ALL
            SELECT schema_name, view_name AS table_name, 'view' AS kind
            FROM duckdb_views()
            WHERE internal = FALSE
        ),
        inconsistencies AS (
            SELECT
                'catalog relation is missing from DuckDB: ' || n.nspname || '.' || c.relname AS message
            FROM rsduck_catalog.rs_relation c
            JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace
            JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
            LEFT JOIN physical_relations physical
              ON lower(physical.schema_name) = lower(n.nspname)
             AND lower(physical.table_name) = lower(c.relname)
             AND (
                (c.relkind = 'r' AND physical.kind = 'table')
                OR (c.relkind IN ('p', 'v') AND physical.kind = 'view')
             )
            WHERE c.status = 'active'
              AND ext.visibility = 'user'
              AND c.relkind IN ('r', 'p', 'v')
              AND {visibility}
              AND physical.table_name IS NULL

            UNION ALL

            SELECT
                'DuckDB relation is missing from catalog: ' || physical.schema_name || '.' || physical.table_name AS message
            FROM physical_relations physical
            LEFT JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(physical.schema_name)
            LEFT JOIN rsduck_catalog.rs_relation c
              ON c.relnamespace = n.oid AND lower(c.relname) = lower(physical.table_name)
            WHERE physical.schema_name NOT IN ('information_schema', 'pg_catalog', 'rsduck_catalog', 'rsduck_internal')
              AND c.oid IS NULL
              AND EXISTS (
                  SELECT 1
                  FROM rsduck_catalog.rs_user_role ur
                  JOIN rsduck_catalog.rs_role role ON role.role_id = ur.role_id
                  WHERE ur.user_id = (
                      SELECT user_id FROM rsduck_catalog.rs_user WHERE username = {username}
                  )
                    AND role.role_name = 'admin'
              )
        )
        SELECT COALESCE((SELECT message FROM inconsistencies ORDER BY message LIMIT 1), '')
        "
    );
    let message: String = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| format!("validate metadata projection failed: {error}"))?;
    if message.is_empty() {
        Ok(())
    } else {
        Err(format!("metadata projection inconsistent: {message}"))
    }
}

fn rewrite_information_schema_sql(sql: &str, username: &str) -> Option<String> {
    let mut rewritten = sql.to_string();
    let mut changed = false;
    for (relation, projection) in [
        ("schemata", information_schema_schemata_sql()),
        ("tables", information_schema_tables_sql(username)),
        ("views", information_schema_views_sql(username)),
        ("routines", information_schema_routines_sql()),
        ("parameters", information_schema_parameters_sql()),
        ("columns", information_schema_columns_sql(username)),
        ("statistics", information_schema_statistics_sql(username)),
        (
            "table_constraints",
            information_schema_table_constraints_sql(username),
        ),
        (
            "key_column_usage",
            information_schema_key_column_usage_sql(username),
        ),
    ] {
        let next = replace_information_schema_relation(&rewritten, relation, &projection);
        if next != rewritten {
            rewritten = next;
            changed = true;
        }
    }
    changed.then_some(rewritten)
}

fn rewrite_mysql_system_sql(sql: &str) -> Option<String> {
    let mut rewritten = sql.to_string();
    let mut changed = false;
    for (relation, projection) in [
        ("role_edges", mysql_role_edges_sql()),
        ("default_roles", mysql_default_roles_sql()),
        ("procs_priv", mysql_procs_priv_sql()),
        ("tables_priv", mysql_tables_priv_sql()),
        ("columns_priv", mysql_columns_priv_sql()),
        ("user", mysql_user_sql()),
        ("db", mysql_db_sql()),
    ] {
        let next = replace_schema_relation(&rewritten, "mysql", relation, &projection);
        if next != rewritten {
            rewritten = next;
            changed = true;
        }
    }
    changed.then_some(rewritten)
}

fn replace_information_schema_relation(sql: &str, relation: &str, projection: &str) -> String {
    replace_schema_relation(sql, "information_schema", relation, projection)
}

fn replace_schema_relation(sql: &str, schema: &str, relation: &str, projection: &str) -> String {
    let replacement = format!("({projection})");
    let mut out = replace_ignore_ascii_case(sql, &format!("{schema}.{relation}"), &replacement);
    out = replace_ignore_ascii_case(&out, &format!("{schema}.`{relation}`"), &replacement);
    out = replace_ignore_ascii_case(&out, &format!("`{schema}`.{relation}"), &replacement);
    replace_ignore_ascii_case(&out, &format!("`{schema}`.`{relation}`"), &replacement)
}

fn mysql_user_sql() -> String {
    "
    WITH accounts AS (
        SELECT
            u.*,
            EXISTS (
                SELECT 1
                FROM rsduck_catalog.rs_user_role ur
                JOIN rsduck_catalog.rs_role role ON role.role_id = ur.role_id
                WHERE ur.user_id = u.user_id AND role.role_name = 'admin'
            ) AS is_admin
        FROM rsduck_catalog.rs_user u
    )
    SELECT
        '%' AS host,
        u.username AS user,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS select_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS insert_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS update_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS delete_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS drop_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS reload_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS shutdown_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS process_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS file_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS grant_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS references_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS index_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS alter_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS show_db_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS super_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_tmp_table_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS lock_tables_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS execute_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS repl_slave_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS repl_client_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_view_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS show_view_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_routine_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS alter_routine_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_user_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS event_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS trigger_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_tablespace_priv,
        '' AS ssl_type,
        '' AS ssl_cipher,
        '' AS x509_issuer,
        '' AS x509_subject,
        0 AS max_questions,
        0 AS max_updates,
        0 AS max_connections,
        0 AS max_user_connections,
        u.mysql_auth_plugin AS plugin,
        u.mysql_auth_string AS authentication_string,
        'N' AS password_expired,
        u.updated_at AS password_last_changed,
        CAST(NULL AS BIGINT) AS password_lifetime,
        CASE WHEN u.status = 'active' THEN 'N' ELSE 'Y' END AS account_locked,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS create_role_priv,
        CASE WHEN u.is_admin THEN 'Y' ELSE 'N' END AS drop_role_priv,
        CAST(NULL AS BIGINT) AS password_reuse_history,
        CAST(NULL AS BIGINT) AS password_reuse_time,
        CAST(NULL AS VARCHAR) AS password_require_current,
        CAST(NULL AS VARCHAR) AS user_attributes
    FROM accounts u
    "
    .to_string()
}

fn mysql_role_edges_sql() -> String {
    "
    SELECT
        '%' AS from_host,
        role.role_name AS from_user,
        '%' AS to_host,
        u.username AS to_user
    FROM rsduck_catalog.rs_user_role ur
    JOIN rsduck_catalog.rs_role role ON role.role_id = ur.role_id
    JOIN rsduck_catalog.rs_user u ON u.user_id = ur.user_id
    ORDER BY role.role_name, u.username
    "
    .to_string()
}

fn mysql_default_roles_sql() -> String {
    "
    SELECT
        '%' AS default_role_host,
        role.role_name AS default_role_user,
        '%' AS host,
        u.username AS user
    FROM rsduck_catalog.rs_user_role ur
    JOIN rsduck_catalog.rs_role role ON role.role_id = ur.role_id
    JOIN rsduck_catalog.rs_user u ON u.user_id = ur.user_id
    ORDER BY role.role_name, u.username
    "
    .to_string()
}

fn mysql_db_sql() -> String {
    "
    WITH schema_privileges AS (
        SELECT
            n.nspname AS db,
            COALESCE(u.username, role.role_name) AS principal_name,
            privilege.action
        FROM rsduck_catalog.rs_privilege privilege
        JOIN rsduck_catalog.rs_schema n ON n.oid = privilege.object_id
        LEFT JOIN rsduck_catalog.rs_user u
          ON privilege.principal_type = 'user' AND u.user_id = privilege.principal_id
        LEFT JOIN rsduck_catalog.rs_role role
          ON privilege.principal_type = 'role' AND role.role_id = privilege.principal_id
        WHERE privilege.object_type = 'schema'
          AND COALESCE(u.username, role.role_name) IS NOT NULL
    )
    SELECT
        '%' AS host,
        db,
        principal_name AS user,
        CASE WHEN max(CASE WHEN action = 'read' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS select_priv,
        CASE WHEN max(CASE WHEN action = 'write' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS insert_priv,
        CASE WHEN max(CASE WHEN action = 'write' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS update_priv,
        CASE WHEN max(CASE WHEN action = 'write' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS delete_priv,
        CASE WHEN max(CASE WHEN action = 'ddl' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS create_priv,
        CASE WHEN max(CASE WHEN action = 'ddl' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS drop_priv,
        'N' AS grant_priv,
        'N' AS references_priv,
        CASE WHEN max(CASE WHEN action = 'ddl' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS index_priv,
        CASE WHEN max(CASE WHEN action = 'ddl' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS alter_priv,
        'N' AS create_tmp_table_priv,
        'N' AS lock_tables_priv,
        CASE WHEN max(CASE WHEN action = 'ddl' THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS create_view_priv,
        CASE WHEN max(CASE WHEN action IN ('read', 'ddl') THEN 1 ELSE 0 END) = 1 THEN 'Y' ELSE 'N' END AS show_view_priv,
        'N' AS create_routine_priv,
        'N' AS alter_routine_priv,
        'N' AS execute_priv,
        'N' AS event_priv,
        'N' AS trigger_priv
    FROM schema_privileges
    GROUP BY db, principal_name
    "
    .to_string()
}

fn mysql_tables_priv_sql() -> String {
    "
    SELECT
        '%' AS host,
        n.nspname AS db,
        COALESCE(u.username, role.role_name) AS user,
        c.relname AS table_name,
        CASE privilege.action
            WHEN 'read' THEN 'Select'
            WHEN 'write' THEN 'Insert,Update,Delete'
            WHEN 'ddl' THEN 'Create,Drop,Index,Alter'
            ELSE ''
        END AS table_priv
    FROM rsduck_catalog.rs_privilege privilege
    JOIN rsduck_catalog.rs_relation c ON c.oid = privilege.object_id
    JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace
    LEFT JOIN rsduck_catalog.rs_user u
      ON privilege.principal_type = 'user' AND u.user_id = privilege.principal_id
    LEFT JOIN rsduck_catalog.rs_role role
      ON privilege.principal_type = 'role' AND role.role_id = privilege.principal_id
    WHERE privilege.object_type = 'relation'
      AND COALESCE(u.username, role.role_name) IS NOT NULL
    "
    .to_string()
}

fn mysql_procs_priv_sql() -> String {
    "
    SELECT
        CAST(NULL AS VARCHAR) AS host,
        CAST(NULL AS VARCHAR) AS db,
        CAST(NULL AS VARCHAR) AS user,
        CAST(NULL AS VARCHAR) AS routine_name,
        CAST(NULL AS VARCHAR) AS routine_type,
        CAST(NULL AS VARCHAR) AS proc_priv
    WHERE FALSE
    "
    .to_string()
}

fn mysql_columns_priv_sql() -> String {
    "
    SELECT
        CAST(NULL AS VARCHAR) AS host,
        CAST(NULL AS VARCHAR) AS db,
        CAST(NULL AS VARCHAR) AS user,
        CAST(NULL AS VARCHAR) AS table_name,
        CAST(NULL AS VARCHAR) AS column_name,
        CAST(NULL AS VARCHAR) AS column_priv
    WHERE FALSE
    "
    .to_string()
}

#[derive(Debug, PartialEq, Eq)]
struct ShowTablesQuery {
    full: bool,
    schema: Option<String>,
    filter: ShowFilter,
}

#[derive(Debug, PartialEq, Eq)]
struct ShowColumnsQuery {
    full: bool,
    schema: Option<String>,
    table: String,
    filter: ShowFilter,
}

#[derive(Debug, PartialEq, Eq)]
struct ShowTableDetailQuery {
    schema: Option<String>,
    table: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ShowIndexQuery {
    schema: Option<String>,
    table: String,
    filter: ShowFilter,
}

#[derive(Debug, PartialEq, Eq)]
enum ShowFilter {
    None,
    Like(String),
    Where(String),
}

fn show_tables_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    let parsed = parse_show_tables_query(sql)?;
    let schema = parsed.schema.as_deref().unwrap_or(current_schema);
    let table_name_column = format!("Tables_in_{schema}");
    let table_name_ident = sql_ident(&table_name_column);
    let columns = if parsed.full {
        format!("table_name AS {table_name_ident}, table_type AS \"Table_type\"")
    } else {
        format!("table_name AS {table_name_ident}")
    };
    let mut out = format!(
        "SELECT * FROM (
            SELECT {columns}
            FROM ({}) information_schema_tables
            WHERE table_catalog = current_database()
              AND table_schema = {}
        ) rsduck_mysql_show_tables",
        information_schema_tables_sql(username),
        sql_literal(schema)
    );
    apply_show_filter(&mut out, &parsed.filter, &table_name_ident);
    out.push_str(&format!(" ORDER BY {table_name_ident}"));
    Some(out)
}

fn show_table_status_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    let parsed = parse_show_table_status_query(sql)?;
    let schema = parsed.schema.as_deref().unwrap_or(current_schema);
    let mut out = format!(
        "SELECT * FROM (
            SELECT
                table_name AS \"Name\",
                engine AS \"Engine\",
                version AS \"Version\",
                row_format AS \"Row_format\",
                table_rows AS \"Rows\",
                avg_row_length AS \"Avg_row_length\",
                data_length AS \"Data_length\",
                max_data_length AS \"Max_data_length\",
                index_length AS \"Index_length\",
                data_free AS \"Data_free\",
                auto_increment AS \"Auto_increment\",
                create_time AS \"Create_time\",
                update_time AS \"Update_time\",
                check_time AS \"Check_time\",
                table_collation AS \"Collation\",
                checksum AS \"Checksum\",
                create_options AS \"Create_options\",
                table_comment AS \"Comment\"
            FROM ({}) information_schema_tables
            WHERE table_catalog = current_database()
              AND table_schema = {}
        ) rsduck_mysql_table_status",
        information_schema_tables_sql(username),
        sql_literal(schema)
    );
    apply_show_filter(&mut out, &parsed.filter, "\"Name\"");
    out.push_str(" ORDER BY \"Name\"");
    Some(out)
}

fn show_table_detail_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    let parsed = parse_show_table_detail_query(sql)?;
    let schema = parsed.schema.as_deref().unwrap_or(current_schema);
    Some(format!(
        "SELECT
            column_name,
            column_type,
            is_nullable AS \"null\",
            column_key AS \"key\",
            CASE WHEN column_default = '' THEN NULL ELSE column_default END AS \"default\",
            extra,
            column_comment AS comment
         FROM ({}) information_schema_columns
         WHERE table_catalog = current_database()
           AND table_schema = {}
           AND table_name = {}
         ORDER BY ordinal_position",
        information_schema_columns_sql(username),
        sql_literal(schema),
        sql_literal(&parsed.table)
    ))
}

fn show_columns_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    let parsed = parse_show_columns_query(sql)?;
    let schema = parsed.schema.as_deref().unwrap_or(current_schema);
    let columns = if parsed.full {
        "\"Field\", \"Type\", \"Collation\", \"Null\", \"Key\", \"Default\", \"Extra\", \"Privileges\", \"Comment\""
    } else {
        "\"Field\", \"Type\", \"Null\", \"Key\", \"Default\", \"Extra\""
    };
    let mut out = format!(
        "SELECT {columns} FROM (
            SELECT
                column_name AS \"Field\",
                ordinal_position AS \"__ordinal\",
                column_type AS \"Type\",
                collation_name AS \"Collation\",
                is_nullable AS \"Null\",
                column_key AS \"Key\",
                CASE WHEN column_default = '' THEN NULL ELSE column_default END AS \"Default\",
                extra AS \"Extra\",
                privileges AS \"Privileges\",
                column_comment AS \"Comment\"
            FROM ({}) information_schema_columns
            WHERE table_catalog = current_database()
              AND table_schema = {}
              AND table_name = {}
        ) rsduck_mysql_show_columns",
        information_schema_columns_sql(username),
        sql_literal(schema),
        sql_literal(&parsed.table)
    );
    apply_show_filter(&mut out, &parsed.filter, "\"Field\"");
    out.push_str(" ORDER BY \"__ordinal\"");
    Some(out)
}

fn show_index_sql(sql: &str, current_schema: &str, username: &str) -> Option<String> {
    let parsed = parse_show_index_query(sql)?;
    let schema = parsed.schema.as_deref().unwrap_or(current_schema);
    let mut out = format!(
        "SELECT * FROM (
            SELECT
                table_name AS \"Table\",
                non_unique AS \"Non_unique\",
                index_name AS \"Key_name\",
                seq_in_index AS \"Seq_in_index\",
                column_name AS \"Column_name\",
                \"collation\" AS \"Collation\",
                cardinality AS \"Cardinality\",
                sub_part AS \"Sub_part\",
                packed AS \"Packed\",
                nullable AS \"Null\",
                index_type AS \"Index_type\",
                \"comment\" AS \"Comment\",
                index_comment AS \"Index_comment\",
                is_visible AS \"Visible\",
                expression AS \"Expression\"
            FROM ({}) information_schema_statistics
            WHERE table_catalog = current_database()
              AND table_schema = {}
              AND table_name = {}
        ) rsduck_mysql_show_index",
        information_schema_statistics_sql(username),
        sql_literal(schema),
        sql_literal(&parsed.table)
    );
    apply_show_filter(&mut out, &parsed.filter, "\"Key_name\"");
    out.push_str(" ORDER BY \"Key_name\", \"Seq_in_index\"");
    Some(out)
}

fn show_routine_status_sql(sql: &str, current_schema: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    let routine_kind = if let Some(next_idx) = consume_keyword(trimmed, idx, "function") {
        idx = skip_mysql_space(trimmed, next_idx);
        "FUNCTION"
    } else if let Some(next_idx) = consume_keyword(trimmed, idx, "procedure") {
        idx = skip_mysql_space(trimmed, next_idx);
        "PROCEDURE"
    } else {
        return None;
    };
    idx = consume_keyword(trimmed, idx, "status")?;
    let filter = parse_show_filter(trimmed, idx)?;

    let source = if routine_kind == "FUNCTION" {
        format!(
            "SELECT DISTINCT
                schema_name AS \"Db\",
                function_name AS \"Name\",
                'FUNCTION' AS \"Type\",
                'admin@%' AS \"Definer\",
                CAST(NULL AS TIMESTAMP) AS \"Modified\",
                CAST(NULL AS TIMESTAMP) AS \"Created\",
                'DEFINER' AS \"Security_type\",
                '' AS \"Comment\",
                'utf8mb4' AS \"character_set_client\",
                'utf8mb4_general_ci' AS \"collation_connection\",
                'utf8mb4_general_ci' AS \"Database Collation\"
             FROM duckdb_functions()
             WHERE function_type IN ('macro', 'table_macro')
               AND database_name = current_database()
               AND schema_name = {}",
            sql_literal(current_schema)
        )
    } else {
        format!(
            "SELECT
                CAST(NULL AS VARCHAR) AS \"Db\",
                CAST(NULL AS VARCHAR) AS \"Name\",
                '{}' AS \"Type\",
                CAST(NULL AS VARCHAR) AS \"Definer\",
                CAST(NULL AS TIMESTAMP) AS \"Modified\",
                CAST(NULL AS TIMESTAMP) AS \"Created\",
                CAST(NULL AS VARCHAR) AS \"Security_type\",
                CAST(NULL AS VARCHAR) AS \"Comment\",
                CAST(NULL AS VARCHAR) AS \"character_set_client\",
                CAST(NULL AS VARCHAR) AS \"collation_connection\",
                CAST(NULL AS VARCHAR) AS \"Database Collation\"
             WHERE FALSE",
            routine_kind
        )
    };
    let mut out = format!("SELECT * FROM ({source}) rsduck_mysql_routine_status");
    apply_show_filter(&mut out, &filter, "\"Name\"");
    out.push_str(" ORDER BY \"Db\", \"Name\"");
    Some(out)
}

fn apply_show_filter(out: &mut String, filter: &ShowFilter, like_column: &str) {
    match filter {
        ShowFilter::None => {}
        ShowFilter::Like(pattern) => {
            out.push_str(&format!(
                " WHERE {like_column} LIKE {}",
                sql_literal(pattern)
            ));
        }
        ShowFilter::Where(expr) => {
            out.push_str(" WHERE ");
            out.push_str(expr);
        }
    }
}

fn parse_show_tables_query(sql: &str) -> Option<ShowTablesQuery> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    let full = if let Some(next_idx) = consume_keyword(trimmed, idx, "full") {
        idx = skip_mysql_space(trimmed, next_idx);
        true
    } else {
        false
    };
    idx = consume_keyword(trimmed, idx, "tables")?;
    idx = skip_mysql_space(trimmed, idx);
    let schema = parse_optional_schema_clause(trimmed, &mut idx)?;
    let filter = parse_show_filter(trimmed, idx)?;
    Some(ShowTablesQuery {
        full,
        schema,
        filter,
    })
}

fn parse_show_table_status_query(sql: &str) -> Option<ShowTablesQuery> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    if let Some(next_idx) = consume_keyword(trimmed, idx, "extended") {
        idx = skip_mysql_space(trimmed, next_idx);
    }
    idx = consume_keyword(trimmed, idx, "table")?;
    idx = skip_mysql_space(trimmed, idx);
    idx = consume_keyword(trimmed, idx, "status")?;
    idx = skip_mysql_space(trimmed, idx);
    let schema = parse_optional_schema_clause(trimmed, &mut idx)?;
    let filter = parse_show_filter(trimmed, idx)?;
    Some(ShowTablesQuery {
        full: false,
        schema,
        filter,
    })
}

fn parse_show_table_detail_query(sql: &str) -> Option<ShowTableDetailQuery> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    idx = consume_keyword(trimmed, idx, "table")?;
    idx = skip_mysql_space(trimmed, idx);
    let (schema, table, next_idx) = parse_mysql_qualified_identifier(trimmed, idx)?;
    if skip_mysql_space(trimmed, next_idx) != trimmed.len() {
        return None;
    }
    Some(ShowTableDetailQuery { schema, table })
}

fn parse_show_columns_query(sql: &str) -> Option<ShowColumnsQuery> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    if let Some(next_idx) =
        consume_keyword(trimmed, idx, "describe").or_else(|| consume_keyword(trimmed, idx, "desc"))
    {
        idx = skip_mysql_space(trimmed, next_idx);
        let (schema, table, next_idx) = parse_mysql_qualified_identifier(trimmed, idx)?;
        idx = skip_mysql_space(trimmed, next_idx);
        let filter = parse_show_filter(trimmed, idx)?;
        return Some(ShowColumnsQuery {
            full: false,
            schema,
            table,
            filter,
        });
    }
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    if let Some(next_idx) = consume_keyword(trimmed, idx, "extended") {
        idx = skip_mysql_space(trimmed, next_idx);
    }
    let full = if let Some(next_idx) = consume_keyword(trimmed, idx, "full") {
        idx = skip_mysql_space(trimmed, next_idx);
        true
    } else {
        false
    };
    idx = consume_keyword(trimmed, idx, "columns")
        .or_else(|| consume_keyword(trimmed, idx, "fields"))?;
    idx = skip_mysql_space(trimmed, idx);
    idx = consume_keyword(trimmed, idx, "from").or_else(|| consume_keyword(trimmed, idx, "in"))?;
    idx = skip_mysql_space(trimmed, idx);
    let (mut schema, table, next_idx) = parse_mysql_qualified_identifier(trimmed, idx)?;
    idx = skip_mysql_space(trimmed, next_idx);
    if schema.is_none() {
        schema = parse_optional_schema_clause(trimmed, &mut idx)?;
    }
    let filter = parse_show_filter(trimmed, idx)?;
    Some(ShowColumnsQuery {
        full,
        schema,
        table,
        filter,
    })
}

fn parse_show_index_query(sql: &str) -> Option<ShowIndexQuery> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut idx = 0;
    idx = consume_keyword(trimmed, idx, "show")?;
    idx = skip_mysql_space(trimmed, idx);
    idx = consume_keyword(trimmed, idx, "index")
        .or_else(|| consume_keyword(trimmed, idx, "indexes"))
        .or_else(|| consume_keyword(trimmed, idx, "keys"))?;
    idx = skip_mysql_space(trimmed, idx);
    idx = consume_keyword(trimmed, idx, "from").or_else(|| consume_keyword(trimmed, idx, "in"))?;
    idx = skip_mysql_space(trimmed, idx);
    let (mut schema, table, next_idx) = parse_mysql_qualified_identifier(trimmed, idx)?;
    idx = skip_mysql_space(trimmed, next_idx);
    if schema.is_none() {
        schema = parse_optional_schema_clause(trimmed, &mut idx)?;
    }
    let filter = parse_show_filter(trimmed, idx)?;
    Some(ShowIndexQuery {
        schema,
        table,
        filter,
    })
}

fn parse_optional_schema_clause(sql: &str, idx: &mut usize) -> Option<Option<String>> {
    if let Some(next_idx) =
        consume_keyword(sql, *idx, "from").or_else(|| consume_keyword(sql, *idx, "in"))
    {
        *idx = skip_mysql_space(sql, next_idx);
        let (schema, next_idx) = parse_mysql_identifier(sql, *idx)?;
        *idx = skip_mysql_space(sql, next_idx);
        Some(Some(schema))
    } else {
        Some(None)
    }
}

fn parse_show_filter(sql: &str, mut idx: usize) -> Option<ShowFilter> {
    idx = skip_mysql_space(sql, idx);
    if idx >= sql.len() {
        return Some(ShowFilter::None);
    }
    if let Some(next_idx) = consume_keyword(sql, idx, "like") {
        idx = skip_mysql_space(sql, next_idx);
        let pattern = parse_single_quoted_literal(&sql[idx..])?;
        Some(ShowFilter::Like(pattern))
    } else if let Some(next_idx) = consume_keyword(sql, idx, "where") {
        let expr = sql[next_idx..].trim();
        if expr.is_empty() {
            None
        } else {
            Some(ShowFilter::Where(expr.to_string()))
        }
    } else {
        None
    }
}

fn parse_mysql_qualified_identifier(
    sql: &str,
    start: usize,
) -> Option<(Option<String>, String, usize)> {
    let (first, mut idx) = parse_mysql_identifier(sql, start)?;
    idx = skip_mysql_space(sql, idx);
    if sql.as_bytes().get(idx) != Some(&b'.') {
        return Some((None, first, idx));
    }
    idx = skip_mysql_space(sql, idx + 1);
    let (second, idx) = parse_mysql_identifier(sql, idx)?;
    Some((Some(first), second, idx))
}

fn information_schema_schemata_sql() -> String {
    "
    SELECT
        current_database() AS catalog_name,
        n.nspname AS schema_name,
        COALESCE(u.username, 'unknown') AS schema_owner,
        current_database() AS default_character_set_catalog,
        'information_schema' AS default_character_set_schema,
        'UTF8' AS default_character_set_name,
        'utf8mb4_general_ci' AS default_collation_name,
        'NO' AS default_encryption,
        '' AS sql_path
    FROM rsduck_catalog.rs_schema n
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = n.nspowner
    WHERE n.nspname NOT IN ('rsduck_catalog', 'rsduck_internal')
    ORDER BY
        CASE WHEN n.nspname = 'main' THEN 0
             WHEN n.nspname = 'information_schema' THEN 2
             ELSE 1
        END,
        n.nspname
    "
    .to_string()
}

fn information_schema_tables_sql(username: &str) -> String {
    format!(
        "
        WITH physical_relations AS (
            SELECT schema_name, table_name, comment, estimated_size
            FROM duckdb_tables()
            WHERE internal = FALSE
            UNION ALL
            SELECT schema_name, view_name AS table_name, comment, CAST(NULL AS BIGINT) AS estimated_size
            FROM duckdb_views()
            WHERE internal = FALSE
        )
        SELECT
            current_database() AS table_catalog,
            physical.schema_name AS table_schema,
            physical.table_name AS table_name,
            CASE WHEN c.relkind = 'v' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type,
            '' AS self_referencing_column_name,
            '' AS reference_generation,
            '' AS user_defined_type_catalog,
            '' AS user_defined_type_schema,
            '' AS user_defined_type_name,
            CASE WHEN c.relkind IN ('r', 'p') THEN 'YES' ELSE 'NO' END AS is_insertable_into,
            'NO' AS is_typed,
            '' AS commit_action,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 'InnoDB' END AS engine,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 10 END AS version,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 'Dynamic' END AS row_format,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE CAST(physical.estimated_size AS BIGINT) END AS table_rows,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 0 END AS avg_row_length,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 0 END AS data_length,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 0 END AS max_data_length,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 0 END AS index_length,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 0 END AS data_free,
            CAST(NULL AS BIGINT) AS auto_increment,
            CAST(NULL AS VARCHAR) AS create_time,
            CAST(NULL AS VARCHAR) AS update_time,
            CAST(NULL AS VARCHAR) AS check_time,
            CASE WHEN c.relkind = 'v' THEN NULL ELSE 'utf8mb4_general_ci' END AS table_collation,
            CAST(NULL AS BIGINT) AS checksum,
            '' AS create_options,
            CASE WHEN c.relkind = 'v' THEN 'VIEW' ELSE COALESCE(NULLIF(physical.comment, ''), d.description, '') END AS table_comment,
            COALESCE(NULLIF(physical.comment, ''), d.description, '') AS description,
            c.status AS rsduck_status,
            c.error_message AS rsduck_error_message
        FROM physical_relations physical
        JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(physical.schema_name)
        JOIN rsduck_catalog.rs_relation c
          ON c.relnamespace = n.oid AND lower(c.relname) = lower(physical.table_name)
        LEFT JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
        LEFT JOIN rsduck_catalog.rs_comment d
          ON d.objoid = c.oid AND d.objsubid = 0
        WHERE c.status IN ('active', 'unavailable')
          AND COALESCE(ext.visibility, 'user') = 'user'
          AND c.relkind IN ('r', 'p', 'v')
          AND {visibility}
        ORDER BY physical.schema_name, physical.table_name
        ",
        visibility = visible_relation_predicate(username, "c", "n"),
    )
}

fn information_schema_views_sql(username: &str) -> String {
    format!(
        "
    SELECT
        current_database() AS table_catalog,
        v.schema_name AS table_schema,
        v.view_name AS table_name,
        v.sql AS view_definition,
        'NONE' AS check_option,
        'YES' AS is_updatable,
        COALESCE(u.username, 'admin') AS definer,
        'DEFINER' AS security_type,
        'utf8mb4' AS character_set_client,
        'utf8mb4_general_ci' AS collation_connection,
        'UNDEFINED' AS algorithm
    FROM duckdb_views() v
    JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(v.schema_name)
    JOIN rsduck_catalog.rs_relation c
      ON c.relnamespace = n.oid AND lower(c.relname) = lower(v.view_name)
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
      AND c.relkind = 'v'
      AND v.schema_name NOT IN ('information_schema', 'rsduck_catalog', 'rsduck_internal')
      AND {visibility}
    ORDER BY v.schema_name, v.view_name
    ",
        visibility = visible_relation_predicate(username, "c", "n"),
    )
}

fn information_schema_routines_sql() -> String {
    "
    SELECT DISTINCT
        current_database() AS specific_catalog,
        f.schema_name AS specific_schema,
        f.function_name AS specific_name,
        current_database() AS routine_catalog,
        f.schema_name AS routine_schema,
        f.function_name AS routine_name,
        'FUNCTION' AS routine_type,
        lower(COALESCE(f.return_type, 'varchar')) AS data_type,
        CAST(NULL AS BIGINT) AS character_maximum_length,
        CAST(NULL AS BIGINT) AS character_octet_length,
        CAST(NULL AS BIGINT) AS numeric_precision,
        CAST(NULL AS BIGINT) AS numeric_scale,
        CAST(NULL AS BIGINT) AS datetime_precision,
        'utf8mb4' AS character_set_name,
        'utf8mb4_general_ci' AS collation_name,
        COALESCE(f.return_type, 'VARCHAR') AS dtd_identifier,
        'SQL' AS routine_body,
        COALESCE(f.macro_definition, '') AS routine_definition,
        '' AS external_name,
        'SQL' AS external_language,
        'SQL' AS parameter_style,
        'NO' AS is_deterministic,
        'CONTAINS SQL' AS sql_data_access,
        CAST(NULL AS VARCHAR) AS sql_path,
        'DEFINER' AS security_type,
        CAST(NULL AS TIMESTAMP) AS created,
        CAST(NULL AS TIMESTAMP) AS last_altered,
        '' AS sql_mode,
        COALESCE(f.comment, '') AS routine_comment,
        'admin@%' AS definer,
        'utf8mb4' AS character_set_client,
        'utf8mb4_general_ci' AS collation_connection,
        'utf8mb4_general_ci' AS database_collation
    FROM duckdb_functions() f
    WHERE f.function_type IN ('macro', 'table_macro')
      AND f.database_name = current_database()
      AND f.schema_name NOT IN ('information_schema', 'rsduck_catalog', 'rsduck_internal')
    ORDER BY f.schema_name, f.function_name
    "
    .to_string()
}

fn information_schema_parameters_sql() -> String {
    "
    SELECT
        current_database() AS specific_catalog,
        f.schema_name AS specific_schema,
        f.function_name AS specific_name,
        parameter.ordinal_position AS ordinal_position,
        'IN' AS parameter_mode,
        parameter.parameter_name AS parameter_name,
        lower(COALESCE(list_extract(f.parameter_types, parameter.ordinal_position), 'varchar')) AS data_type,
        CAST(NULL AS BIGINT) AS character_maximum_length,
        CAST(NULL AS BIGINT) AS character_octet_length,
        CAST(NULL AS BIGINT) AS numeric_precision,
        CAST(NULL AS BIGINT) AS numeric_scale,
        CAST(NULL AS BIGINT) AS datetime_precision,
        'utf8mb4' AS character_set_name,
        'utf8mb4_general_ci' AS collation_name,
        COALESCE(list_extract(f.parameter_types, parameter.ordinal_position), 'VARCHAR') AS dtd_identifier,
        'FUNCTION' AS routine_type
    FROM duckdb_functions() f
    CROSS JOIN UNNEST(f.parameters) WITH ORDINALITY AS parameter(parameter_name, ordinal_position)
    WHERE f.function_type IN ('macro', 'table_macro')
      AND f.database_name = current_database()
      AND f.schema_name NOT IN ('information_schema', 'rsduck_catalog', 'rsduck_internal')
    ORDER BY f.schema_name, f.function_name, parameter.ordinal_position
    "
    .to_string()
}

fn information_schema_columns_sql(username: &str) -> String {
    format!(
        "
        SELECT
            current_database() AS table_catalog,
            physical.schema_name AS table_schema,
            physical.table_name AS table_name,
            physical.column_name AS column_name,
            physical.column_index AS ordinal_position,
            COALESCE(physical.column_default, '') AS column_default,
            CASE WHEN physical.is_nullable THEN 'YES' ELSE 'NO' END AS is_nullable,
            lower(physical.data_type) AS data_type,
            physical.character_maximum_length AS character_maximum_length,
            physical.character_maximum_length AS character_octet_length,
            physical.numeric_precision AS numeric_precision,
            physical.numeric_precision_radix AS numeric_precision_radix,
            physical.numeric_scale AS numeric_scale,
            CASE WHEN lower(physical.data_type) IN ('time', 'timestamp', 'timestamp with time zone') THEN 6 ELSE NULL END AS datetime_precision,
            '' AS character_set_name,
            '' AS collation_name,
            lower(physical.data_type) AS column_type,
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM rsduck_catalog.rs_constraint con
                    WHERE con.conrelid = c.oid AND con.contype = 'p'
                      AND COALESCE(list_position(string_split(con.conkey, ','), CAST(physical.column_index AS VARCHAR)), 0) > 0
                ) THEN 'PRI'
                WHEN EXISTS (
                    SELECT 1 FROM rsduck_catalog.rs_constraint con
                    WHERE con.conrelid = c.oid AND con.contype = 'u'
                      AND COALESCE(list_position(string_split(con.conkey, ','), CAST(physical.column_index AS VARCHAR)), 0) > 0
                ) THEN 'UNI'
                ELSE ''
            END AS column_key,
            '' AS extra,
            '' AS privileges,
            COALESCE(NULLIF(physical.comment, ''), d.description, '') AS column_comment,
            'NEVER' AS generation_expression,
            'YES' AS is_updatable
        FROM duckdb_columns() physical
        JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(physical.schema_name)
        JOIN rsduck_catalog.rs_relation c
          ON c.relnamespace = n.oid AND lower(c.relname) = lower(physical.table_name)
        JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
        LEFT JOIN rsduck_catalog.rs_comment d
          ON d.objoid = c.oid AND d.objsubid = physical.column_index
        WHERE physical.internal = FALSE
          AND c.status IN ('active', 'unavailable')
          AND ext.visibility = 'user'
          AND c.relkind IN ('r', 'p', 'v')
          AND {visibility}
        ORDER BY physical.schema_name, physical.table_name, physical.column_index
        ",
        visibility = visible_relation_predicate(username, "c", "n"),
    )
}

fn information_schema_statistics_sql(username: &str) -> String {
    format!(
        "
        SELECT
            current_database() AS table_catalog,
            physical.schema_name AS table_schema,
            physical.table_name AS table_name,
            CASE WHEN physical.is_unique THEN 0 ELSE 1 END AS non_unique,
            physical.schema_name AS index_schema,
            physical.index_name AS index_name,
            1 AS seq_in_index,
            NULLIF(regexp_extract(physical.sql, '\\((.*)\\)', 1), '') AS column_name,
            'A' AS collation,
            CAST(NULL AS BIGINT) AS cardinality,
            CAST(NULL AS BIGINT) AS sub_part,
            CAST(NULL AS VARCHAR) AS packed,
            'YES' AS nullable,
            'BTREE' AS index_type,
            '' AS comment,
            COALESCE(physical.comment, '') AS index_comment,
            'YES' AS is_visible,
            CAST(NULL AS VARCHAR) AS expression
        FROM duckdb_indexes() physical
        JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(physical.schema_name)
        JOIN rsduck_catalog.rs_relation c
          ON c.relnamespace = n.oid AND lower(c.relname) = lower(physical.table_name)
        JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
        WHERE c.status IN ('active', 'unavailable')
          AND ext.visibility = 'user'
          AND c.relkind IN ('r', 'p')
          AND {visibility}
        ORDER BY physical.schema_name, physical.table_name, physical.index_name
        ",
        visibility = visible_relation_predicate(username, "c", "n"),
    )
}

fn information_schema_table_constraints_sql(username: &str) -> String {
    format!(
        "
    SELECT
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        CASE con.contype
            WHEN 'p' THEN 'PRIMARY KEY'
            WHEN 'u' THEN 'UNIQUE'
            WHEN 'f' THEN 'FOREIGN KEY'
            WHEN 'c' THEN 'CHECK'
            ELSE con.contype
        END AS constraint_type,
        CASE WHEN con.convalidated THEN 'YES' ELSE 'NO' END AS is_deferrable,
        'NO' AS initially_deferred,
        CASE WHEN con.convalidated THEN 'YES' ELSE 'NO' END AS enforced
    FROM rsduck_catalog.rs_constraint con
    JOIN rsduck_catalog.rs_schema n ON n.oid = con.connamespace
    JOIN rsduck_catalog.rs_relation tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.rs_schema tn ON tn.oid = tc.relnamespace
    WHERE tc.status IN ('active', 'unavailable')
      AND {visibility}
    ORDER BY tn.nspname, tc.relname, con.conname
    ",
        visibility = visible_relation_predicate(username, "tc", "tn"),
    )
}

fn information_schema_key_column_usage_sql(username: &str) -> String {
    format!(
        "
    SELECT
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        key_pos.key_index AS ordinal_position,
        key_pos.key_index AS position_in_unique_constraint,
        current_database() AS referenced_table_catalog,
        rn.nspname AS referenced_table_schema,
        rc.relname AS referenced_table_name,
        ra.attname AS referenced_column_name
    FROM rsduck_catalog.rs_constraint con
    JOIN rsduck_catalog.rs_schema n ON n.oid = con.connamespace
    JOIN rsduck_catalog.rs_relation tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.rs_schema tn ON tn.oid = tc.relnamespace
    JOIN UNNEST(string_split(con.conkey, ',')) WITH ORDINALITY AS key_pos(attnum_text, key_index)
      ON TRUE
    JOIN rsduck_catalog.rs_column a
      ON a.attrelid = tc.oid AND CAST(a.attnum AS VARCHAR) = key_pos.attnum_text
    LEFT JOIN rsduck_catalog.rs_relation rc ON rc.oid = con.confrelid
    LEFT JOIN rsduck_catalog.rs_schema rn ON rn.oid = rc.relnamespace
    LEFT JOIN rsduck_catalog.rs_column ra
      ON ra.attrelid = rc.oid
     AND CAST(ra.attnum AS VARCHAR) = list_extract(string_split(con.confkey, ','), key_pos.key_index)
    WHERE tc.status IN ('active', 'unavailable')
      AND con.contype IN ('p', 'u', 'f')
      AND {visibility}
    ORDER BY tn.nspname, tc.relname, con.conname, key_pos.key_index
    ",
        visibility = visible_relation_predicate(username, "tc", "tn"),
    )
}

fn visible_relation_predicate(username: &str, relation_alias: &str, schema_alias: &str) -> String {
    let username = sql_literal(username);
    format!(
        "(
            EXISTS (
                SELECT 1
                FROM rsduck_catalog.rs_user_role ur
                JOIN rsduck_catalog.rs_role role ON role.role_id = ur.role_id
                WHERE ur.user_id = (
                    SELECT user_id FROM rsduck_catalog.rs_user WHERE username = {username}
                )
                  AND role.role_name = 'admin'
            )
            OR EXISTS (
                SELECT 1
                FROM rsduck_catalog.rs_privilege privilege
                WHERE privilege.action = 'read'
                  AND (
                    (privilege.object_type = 'relation' AND privilege.object_id = {relation_alias}.oid)
                    OR (privilege.object_type = 'schema' AND privilege.object_id = {schema_alias}.oid)
                  )
                  AND (
                    (privilege.principal_type = 'user' AND privilege.principal_id = (
                        SELECT user_id FROM rsduck_catalog.rs_user WHERE username = {username}
                    ))
                    OR (
                        privilege.principal_type = 'role'
                        AND privilege.principal_id IN (
                            SELECT ur.role_id
                            FROM rsduck_catalog.rs_user_role ur
                            JOIN rsduck_catalog.rs_user user_account ON user_account.user_id = ur.user_id
                            WHERE user_account.username = {username}
                        )
                    )
                  )
            )
        )"
    )
}

fn consume_keyword(sql: &str, idx: usize, keyword: &str) -> Option<usize> {
    let end = idx.checked_add(keyword.len())?;
    if end <= sql.len()
        && sql[idx..end].eq_ignore_ascii_case(keyword)
        && (idx == 0 || !is_mysql_ident_byte(sql.as_bytes()[idx - 1]))
        && (end == sql.len() || !is_mysql_ident_byte(sql.as_bytes()[end]))
    {
        Some(end)
    } else {
        None
    }
}

fn parse_mysql_identifier(sql: &str, start: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    if bytes.get(start) == Some(&b'`') {
        let mut idx = start + 1;
        let mut out = String::new();
        while idx < bytes.len() {
            if bytes[idx] == b'`' {
                if bytes.get(idx + 1) == Some(&b'`') {
                    out.push('`');
                    idx += 2;
                    continue;
                }
                return Some((out, idx + 1));
            }
            out.push(bytes[idx] as char);
            idx += 1;
        }
        return None;
    }

    let mut idx = start;
    while idx < bytes.len() && is_mysql_ident_byte(bytes[idx]) {
        idx += 1;
    }
    (idx > start).then(|| (sql[start..idx].to_string(), idx))
}

fn skip_mysql_space(sql: &str, mut idx: usize) -> usize {
    while sql
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        idx += 1;
    }
    idx
}

fn is_mysql_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn sql_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn parse_single_quoted_literal(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut out = String::new();
    let mut idx = 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' if bytes.get(idx + 1) == Some(&b'\'') => {
                out.push('\'');
                idx += 2;
            }
            b'\'' => {
                if input[idx + 1..].trim().is_empty() {
                    return Some(out);
                }
                return None;
            }
            _ => {
                let next = next_sql_char_index(input, idx);
                out.push_str(&input[idx..next]);
                idx = next;
            }
        }
    }
    None
}

fn next_sql_char_index(sql: &str, idx: usize) -> usize {
    idx + sql[idx..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn replace_ignore_ascii_case(input: &str, needle: &str, replacement: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if input[idx..]
            .get(..needle.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
        {
            output.push_str(replacement);
            idx += needle.len();
        } else {
            let ch = input[idx..].chars().next().expect("valid char boundary");
            output.push(ch);
            idx += ch.len_utf8();
        }
    }
    output
}

fn references_information_schema_engines(sql: &str) -> bool {
    sql.chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '`' && *ch != '"')
        .collect::<String>()
        .to_ascii_lowercase()
        .contains("information_schema.engines")
}

fn mysql_engines_result() -> SqlTypedResult {
    SqlTypedResult::Query {
        columns: vec![
            SqlColumn {
                name: "ENGINE".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "SUPPORT".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "COMMENT".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "TRANSACTIONS".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "XA".to_string(),
                data_type: SqlType::Text,
            },
            SqlColumn {
                name: "SAVEPOINTS".to_string(),
                data_type: SqlType::Text,
            },
        ],
        rows: vec![vec![
            SqlValue::Text("InnoDB".to_string()),
            SqlValue::Text("DEFAULT".to_string()),
            SqlValue::Text("rsduck MySQL protocol compatibility engine".to_string()),
            SqlValue::Text("YES".to_string()),
            SqlValue::Text("NO".to_string()),
            SqlValue::Text("YES".to_string()),
        ]],
    }
}

#[cfg(test)]
mod tests {
    use super::{is_show_table_detail, replace_ignore_ascii_case, rewrite_sql};
    use duckdb::Connection;

    #[test]
    fn replace_ignore_ascii_case_is_utf8_boundary_safe() {
        let sql = "INSERT INTO sector_list VALUES ('GN_SEMI', '半导体')";
        assert_eq!(
            replace_ignore_ascii_case(sql, "information_schema.tables", "x"),
            sql
        );
    }

    #[test]
    fn projects_duckdb_macros_as_mysql_routines_and_parameters() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE MACRO add_one(value) AS value + 1")
            .unwrap();

        let routines_sql = rewrite_sql(
            "SELECT routine_schema, routine_name, routine_type FROM information_schema.routines",
            "main",
            "admin",
        )
        .unwrap();
        let routine: (String, String, String) = conn
            .query_row(&routines_sql, [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            routine,
            (
                "main".to_string(),
                "add_one".to_string(),
                "FUNCTION".to_string()
            )
        );

        let parameters_sql = rewrite_sql(
            "SELECT specific_schema, specific_name, parameter_name FROM information_schema.parameters",
            "main",
            "admin",
        )
        .unwrap();
        let parameter: (String, String, String) = conn
            .query_row(&parameters_sql, [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            parameter,
            (
                "main".to_string(),
                "add_one".to_string(),
                "value".to_string()
            )
        );
    }

    #[test]
    fn show_table_detail_rewrites_to_column_metadata_with_comments() {
        assert!(is_show_table_detail("SHOW TABLE main.sector_list"));
        assert!(!is_show_table_detail("SHOW TABLE STATUS"));
        assert!(!is_show_table_detail("SHOW TABLES"));

        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE sector_list(sector_code VARCHAR)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON COLUMN sector_list.sector_code IS 'global sector code'",
        )
        .unwrap();

        let sql = rewrite_sql("SHOW TABLE main.sector_list", "main", "admin").unwrap();
        assert!(sql.contains("column_comment AS comment"));
        assert!(sql.contains("table_schema = 'main'"));
        assert!(sql.contains("table_name = 'sector_list'"));
        let (column_name, comment): (String, String) = conn
            .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(6)?)))
            .unwrap();
        assert_eq!(column_name, "sector_code");
        assert_eq!(comment, "global sector code");
    }
}

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use duckdb::Connection;
use rand_core::OsRng;
use sqlparser::ast::{
    Action, AlterTable, AlterTableOperation, AlterUser, ColumnOption, CommentObject, CreateIndex,
    CreateTable, CreateUser, CreateView, Expr, Grant, GrantObjects, GranteeName, GranteesType,
    Insert, ObjectName, ObjectNamePart, ObjectType, Privileges, Revoke, SchemaName, SetExpr,
    Statement, TableConstraint, TableObject, Value,
};
use sqlparser::dialect::{DuckDbDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use tracing::warn;

const CATALOG_VERSION: i64 = 1;

const ADMIN_USER_ID: i64 = 10;
const PG_CATALOG_NS: i64 = 11;
const INFORMATION_SCHEMA_NS: i64 = 12;
const RSDUCK_CATALOG_NS: i64 = 13;
const RSDUCK_INTERNAL_NS: i64 = 14;
const MAIN_NS: i64 = 15;

const ROLE_ADMIN_ID: i64 = 20;
const ROLE_OPERATOR_ID: i64 = 21;
const ROLE_DDL_ID: i64 = 22;
const ROLE_WRITER_ID: i64 = 23;
const ROLE_READER_ID: i64 = 24;

const FIRST_USER_OID: i64 = 10_000;
const PG_CLASS_CLASSOID: i64 = 1259;
const PG_NAMESPACE_CLASSOID: i64 = 2615;

#[derive(Debug, Clone)]
struct CatalogColumn {
    name: String,
    pg_type_oid: i64,
    attnum: i32,
    not_null: bool,
    default_expr: Option<String>,
}

#[derive(Debug, Clone)]
struct RelationMeta {
    oid: i64,
    reltype: i64,
    relkind: String,
    relispartition: bool,
}

#[derive(Debug, Clone)]
struct ManagedPartitionCreate {
    base_sql: String,
    partition_key: String,
    partition_unit: String,
    retention_count: i32,
}

#[derive(Debug, Clone)]
struct PartitionedRelation {
    oid: i64,
    schema: String,
    name: String,
    partition_key: String,
    partition_key_type: String,
    partition_unit: String,
    columns: Vec<CatalogColumn>,
}

#[derive(Debug, Clone)]
struct PartitionRoute {
    partition_value: String,
    route_ts: Option<NaiveDateTime>,
}

#[derive(Debug, Clone)]
struct PartitionBounds {
    value: String,
    lower_bound: NaiveDateTime,
    upper_bound: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct SessionPrincipal {
    pub user_id: i64,
    pub username: String,
    pub roles: Vec<String>,
}

pub fn bootstrap_fresh(conn: &Connection) -> Result<(), String> {
    create_catalog_storage(conn)?;
    if catalog_version_row_exists(conn)? {
        return Ok(());
    }
    insert_bootstrap_rows(conn)
}

pub fn validate_after_start(conn: &Connection) -> Result<(), String> {
    if !catalog_exists(conn)? {
        if has_user_objects(conn)? {
            return Err(
                "rsduck catalog is missing but DuckDB already contains user objects".into(),
            );
        }
        bootstrap_fresh(conn)?;
    }

    let version: i64 = conn
        .query_row(
            "SELECT schema_version FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read catalog version failed: {e}"))?;
    if version != CATALOG_VERSION {
        return Err(format!(
            "unsupported rsduck catalog schema version: {version}, expected {CATALOG_VERSION}"
        ));
    }

    let status: String = conn
        .query_row(
            "SELECT status FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read catalog status failed: {e}"))?;
    if status != "ready" {
        return Err(format!("rsduck catalog status is not ready: {status}"));
    }

    validate_physical_relations(conn)?;
    validate_partitioned_relations(conn)?;
    Ok(())
}

fn validate_physical_relations(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, n.nspname, c.relname, c.relkind \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.status = 'active' AND c.relkind IN ('r', 'v', 'i') \
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare catalog physical validation failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query catalog physical validation failed: {e}"))?;
    let mut relations = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read catalog physical validation failed: {e}"))?
    {
        relations.push((
            row.get::<_, i64>(0)
                .map_err(|e| format!("read rel oid failed: {e}"))?,
            row.get::<_, String>(1)
                .map_err(|e| format!("read rel schema failed: {e}"))?,
            row.get::<_, String>(2)
                .map_err(|e| format!("read rel name failed: {e}"))?,
            row.get::<_, String>(3)
                .map_err(|e| format!("read rel kind failed: {e}"))?,
        ));
    }

    for (rel_oid, schema, relname, relkind) in relations {
        let validation = match relkind.as_str() {
            "r" => validate_table_physical(conn, rel_oid, &schema, &relname),
            "v" => validate_view_physical(conn, rel_oid, &schema, &relname),
            "i" => validate_index_physical(conn, &schema, &relname),
            _ => Ok(()),
        };

        if let Err(reason) = validation {
            warn!(
                "Catalog relation unavailable after startup validation: {}.{}: {}",
                schema, relname, reason
            );
            mark_relation_unavailable(conn, rel_oid, &reason)?;
        }
    }

    Ok(())
}

fn validate_partitioned_relations(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, n.nspname, c.relname \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.status = 'active' AND c.relkind = 'p' \
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare partitioned relation validation failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partitioned relation validation failed: {e}"))?;
    let mut parents = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partitioned relation validation failed: {e}"))?
    {
        parents.push((
            row.get::<_, i64>(0)
                .map_err(|e| format!("read partitioned rel oid failed: {e}"))?,
            row.get::<_, String>(1)
                .map_err(|e| format!("read partitioned rel schema failed: {e}"))?,
            row.get::<_, String>(2)
                .map_err(|e| format!("read partitioned rel name failed: {e}"))?,
        ));
    }

    for (parent_oid, schema, relname) in parents {
        if let Err(reason) = validate_partitioned_relation(conn, parent_oid, &schema, &relname) {
            warn!(
                "Catalog partitioned relation unavailable after startup validation: {}.{}: {}",
                schema, relname, reason
            );
            mark_relation_unavailable(conn, parent_oid, &reason)?;
        }
    }
    Ok(())
}

fn validate_partitioned_relation(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let partitions = active_partition_children(conn, parent_oid)?;
    if partitions.is_empty() {
        return Err("managed partitioned table has no active partitions".into());
    }

    let mut active_physical = Vec::with_capacity(partitions.len());
    for partition in &partitions {
        if partition.child_status != "active" {
            let reason = format!(
                "active partition child is not active: {}.{} status={}",
                partition.schema, partition.relname, partition.child_status
            );
            mark_partition_failed(conn, parent_oid, partition.child_oid, &reason)?;
            return Err(reason);
        }
        if let Err(reason) = validate_table_physical(
            conn,
            partition.child_oid,
            &partition.schema,
            &partition.relname,
        ) {
            mark_relation_unavailable(conn, partition.child_oid, &reason)?;
            mark_partition_failed(conn, parent_oid, partition.child_oid, &reason)?;
            return Err(format!(
                "active partition child unavailable: {}.{}: {reason}",
                partition.schema, partition.relname
            ));
        }
        active_physical.push((partition.schema.as_str(), partition.relname.as_str()));
    }

    let expected_sql = partition_entrypoint_sql(schema, relname, &active_physical);
    let generated_sql: String = conn
        .query_row(
            &format!(
                "SELECT generated_sql FROM rsduck_catalog.rs_relation_ext WHERE relid = {parent_oid}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read partition entrypoint SQL failed: {e}"))?;

    if generated_sql.trim() != expected_sql {
        rebuild_partition_entrypoint(conn, parent_oid, &expected_sql)?;
    } else if validate_view_physical(conn, parent_oid, schema, relname).is_err() {
        rebuild_partition_entrypoint(conn, parent_oid, &expected_sql)?;
    }
    validate_view_physical(conn, parent_oid, schema, relname)
}

#[derive(Debug)]
struct ActivePartitionChild {
    child_oid: i64,
    schema: String,
    relname: String,
    child_status: String,
}

fn active_partition_children(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<ActivePartitionChild>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.child_relid, n.nspname, c.relname, c.status \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} AND p.status = 'active' \
             ORDER BY p.is_null_partition, p.partition_value"
        ))
        .map_err(|e| format!("prepare active partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query active partition lookup failed: {e}"))?;
    let mut partitions = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read active partition lookup failed: {e}"))?
    {
        partitions.push(ActivePartitionChild {
            child_oid: row
                .get(0)
                .map_err(|e| format!("read active partition child oid failed: {e}"))?,
            schema: row
                .get(1)
                .map_err(|e| format!("read active partition schema failed: {e}"))?,
            relname: row
                .get(2)
                .map_err(|e| format!("read active partition relation failed: {e}"))?,
            child_status: row
                .get(3)
                .map_err(|e| format!("read active partition child status failed: {e}"))?,
        });
    }
    Ok(partitions)
}

fn rebuild_partition_entrypoint(
    conn: &Connection,
    parent_oid: i64,
    expected_sql: &str,
) -> Result<(), String> {
    conn.execute(expected_sql, [])
        .map_err(|e| format!("rebuild partition entrypoint failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation_ext \
             SET generated_sql = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE relid = {parent_oid}",
            sql_string(expected_sql)
        ),
        [],
    )
    .map_err(|e| format!("record rebuilt partition entrypoint failed: {e}"))?;
    Ok(())
}

fn mark_partition_failed(
    conn: &Connection,
    parent_oid: i64,
    child_oid: i64,
    reason: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET status = 'failed', error_message = '{}' \
             WHERE parent_relid = {parent_oid} AND child_relid = {child_oid}",
            sql_string(reason)
        ),
        [],
    )
    .map_err(|e| format!("mark partition failed failed: {e}"))?;
    Ok(())
}

fn validate_table_physical(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let count = count_duckdb_relation(conn, "duckdb_tables()", "table_name", schema, relname)?;
    if count == 0 {
        return Err("missing DuckDB physical table".into());
    }
    validate_catalog_columns_match_duckdb(conn, rel_oid, schema, relname)
}

fn validate_view_physical(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let count = count_duckdb_relation(conn, "duckdb_views()", "view_name", schema, relname)?;
    if count == 0 {
        return Err("missing DuckDB physical view".into());
    }
    validate_catalog_columns_match_duckdb(conn, rel_oid, schema, relname)
}

fn validate_index_physical(conn: &Connection, schema: &str, relname: &str) -> Result<(), String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM duckdb_indexes() \
                 WHERE schema_name = '{}' AND index_name = '{}'",
                sql_string(schema),
                sql_string(relname)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("query DuckDB physical index failed: {e}"))?;
    if count == 0 {
        return Err("missing DuckDB physical index".into());
    }
    Ok(())
}

fn validate_catalog_columns_match_duckdb(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let catalog = catalog_columns(conn, rel_oid)?;
    let physical = load_duckdb_columns(conn, schema, relname)?;
    if catalog.len() != physical.len() {
        return Err(format!(
            "column count mismatch: catalog={}, duckdb={}",
            catalog.len(),
            physical.len()
        ));
    }
    for (catalog_column, physical_column) in catalog.iter().zip(physical.iter()) {
        if !catalog_column
            .name
            .eq_ignore_ascii_case(&physical_column.name)
            || catalog_column.pg_type_oid != physical_column.pg_type_oid
            || catalog_column.attnum != physical_column.attnum
        {
            return Err(format!(
                "column mismatch at attnum {}: catalog={} duckdb={}",
                catalog_column.attnum, catalog_column.name, physical_column.name
            ));
        }
    }
    Ok(())
}

fn count_duckdb_relation(
    conn: &Connection,
    table_function: &str,
    name_column: &str,
    schema: &str,
    relname: &str,
) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {table_function} \
             WHERE schema_name = '{}' AND {name_column} = '{}' AND internal = FALSE",
            sql_string(schema),
            sql_string(relname)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("query DuckDB physical relation failed: {e}"))
}

fn mark_relation_unavailable(conn: &Connection, rel_oid: i64, reason: &str) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.pg_class \
             SET status = 'unavailable', error_message = '{}' \
             WHERE oid = {rel_oid}",
            sql_string(reason)
        ),
        [],
    )
    .map_err(|e| format!("mark relation unavailable failed: {e}"))?;
    Ok(())
}

pub fn execute_init_sql(conn: &Connection, sql: &str) -> Result<(), String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("init_sql parse failed: {e}"))?;
    for statement in statements {
        let sql = statement.to_string();
        execute_catalog_statement(conn, &statement, &sql, ADMIN_USER_ID)?;
    }
    Ok(())
}

pub fn execute_catalog_aware_write(conn: &Connection, sql: &str) -> Result<Option<usize>, String> {
    execute_catalog_aware_write_as(conn, "admin", sql)
}

pub fn execute_catalog_aware_write_as(
    conn: &Connection,
    username: &str,
    sql: &str,
) -> Result<Option<usize>, String> {
    let principal = principal_for_username(conn, username)?;
    if let Some(partitioned) = parse_managed_partition_create(sql)? {
        return create_range_partitioned_table(conn, &partitioned, principal.user_id).map(Some);
    }
    let (statement, normalized_sql) = parse_one_statement(sql)?;
    execute_catalog_statement(conn, &statement, &normalized_sql, principal.user_id)
}

pub fn guard_external_sql(sql: &str) -> Result<(), String> {
    let normalized = normalize_for_guard(sql);
    if normalized.contains("rsduck_catalog.") || normalized.contains("rsduck_internal.") {
        return Err("reserved schema is managed by rsduck catalog".into());
    }
    Ok(())
}

pub fn looks_like_managed_partition_create(sql: &str) -> bool {
    normalize_for_guard(sql).starts_with("create table ")
        && find_keyword_phrase(sql, "partition by range").is_some()
}

pub fn authorize_sql(conn: &Connection, username: &str, sql: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    if principal.is_admin() {
        return Ok(());
    }

    if let Some(partitioned) = parse_managed_partition_create(sql)? {
        let (statement, _) = parse_one_statement(&partitioned.base_sql)?;
        let Statement::CreateTable(create_table) = statement else {
            return Err("managed partitioned table base DDL must be CREATE TABLE".into());
        };
        let (schema, _) = relation_name(&create_table.name)?;
        return require_schema_action(conn, &principal, &schema, "ddl");
    }

    let (statement, normalized_sql) = parse_one_statement(sql)?;
    match statement {
        Statement::Query(_) | Statement::ExplainTable { .. } | Statement::Explain { .. } => {
            for relation in extract_read_relations(&normalized_sql) {
                require_relation_action(conn, &principal, &relation, "read")?;
            }
            Ok(())
        }
        Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCharset(_)
        | Statement::ShowObjects(_)
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowCollation { .. } => Ok(()),
        Statement::Insert(_) => {
            let relation = extract_relation_after(&normalized_sql, "into")
                .ok_or_else(|| "cannot determine INSERT target relation".to_string())?;
            require_relation_action(conn, &principal, &relation, "write")
        }
        Statement::Update(_) => {
            let relation = extract_relation_after(&normalized_sql, "update")
                .ok_or_else(|| "cannot determine UPDATE target relation".to_string())?;
            require_relation_action(conn, &principal, &relation, "write")
        }
        Statement::Delete(_) => {
            let relation = extract_relation_after(&normalized_sql, "from")
                .ok_or_else(|| "cannot determine DELETE target relation".to_string())?;
            require_relation_action(conn, &principal, &relation, "write")
        }
        Statement::Copy { to, .. } => {
            let relation = extract_relation_after(&normalized_sql, "copy")
                .ok_or_else(|| "cannot determine COPY target relation".to_string())?;
            if to {
                require_relation_action(conn, &principal, &relation, "read")
            } else {
                require_relation_action(conn, &principal, &relation, "write")
            }
        }
        Statement::CreateSchema { .. } => require_system_action(conn, &principal, "manage_catalog"),
        Statement::CreateUser(_)
        | Statement::AlterUser(_)
        | Statement::Grant(_)
        | Statement::Revoke(_) => require_system_action(conn, &principal, "manage_user"),
        Statement::CreateTable(create_table) => {
            let (schema, _) = relation_name(&create_table.name)?;
            require_schema_action(conn, &principal, &schema, "ddl")
        }
        Statement::CreateView(create_view) => {
            let (schema, _) = relation_name(&create_view.name)?;
            require_schema_action(conn, &principal, &schema, "ddl")
        }
        Statement::CreateIndex(create_index) => {
            let (schema, relation) = relation_name(&create_index.table_name)?;
            require_relation_action(conn, &principal, &(schema, relation), "ddl")
        }
        Statement::Comment {
            object_type,
            object_name,
            ..
        } => match object_type {
            CommentObject::Schema => {
                let schema = single_name_part(&object_name)?;
                require_schema_action(conn, &principal, &schema, "ddl")
            }
            CommentObject::Table
            | CommentObject::View
            | CommentObject::Index
            | CommentObject::Column => {
                let relation = comment_relation_name(object_type, &object_name)?;
                require_relation_action(conn, &principal, &relation, "ddl")
            }
            _ => Err(format!("COMMENT ON {object_type} is not supported")),
        },
        Statement::Drop { .. }
        | Statement::AlterTable(_)
        | Statement::AlterSchema(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. } => {
            if let Some(relation) = extract_first_relation_for_ddl(&normalized_sql) {
                require_relation_action(conn, &principal, &relation, "ddl")
            } else {
                require_system_action(conn, &principal, "manage_catalog")
            }
        }
        Statement::Set(_) | Statement::Use(_) | Statement::Pragma { .. } => Ok(()),
        _ => Err(format!(
            "permission denied for statement type: {}",
            statement_command(&statement)
        )),
    }
}

pub fn authorize_snapshot(conn: &Connection, username: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    require_system_action(conn, &principal, "manage_snapshot")
}

pub fn looks_like_privilege_function(sql: &str) -> bool {
    let normalized = normalize_for_guard(sql);
    normalized.starts_with("select ")
        && (normalized.contains("has_table_privilege(")
            || normalized.contains("has_schema_privilege(")
            || normalized.contains("has_database_privilege(")
            || normalized.contains("pg_catalog.has_table_privilege(")
            || normalized.contains("pg_catalog.has_schema_privilege(")
            || normalized.contains("pg_catalog.has_database_privilege("))
}

pub fn evaluate_privilege_function(
    conn: &Connection,
    current_user: &str,
    sql: &str,
) -> Result<(String, bool), String> {
    let normalized = normalize_for_guard(sql);
    let args = quoted_literals(sql);
    if normalized.contains("has_table_privilege(")
        || normalized.contains("pg_catalog.has_table_privilege(")
    {
        let (target_user, relation, privilege) = privilege_args(current_user, &args)?;
        let action = table_privilege_action(&privilege);
        let relation = relation_from_token(&relation)
            .ok_or_else(|| format!("invalid relation name in has_table_privilege: {relation}"))?;
        return Ok((
            "has_table_privilege".to_string(),
            has_relation_action(conn, &target_user, &relation, action)?,
        ));
    }
    if normalized.contains("has_schema_privilege(")
        || normalized.contains("pg_catalog.has_schema_privilege(")
    {
        let (target_user, schema, privilege) = privilege_args(current_user, &args)?;
        let action = schema_privilege_action(&privilege);
        return Ok((
            "has_schema_privilege".to_string(),
            has_schema_action(conn, &target_user, &schema, action)?,
        ));
    }
    if normalized.contains("has_database_privilege(")
        || normalized.contains("pg_catalog.has_database_privilege(")
    {
        let target_user = if args.len() >= 3 {
            args[0].clone()
        } else {
            current_user.to_string()
        };
        let principal = principal_for_username(conn, &target_user)?;
        return Ok(("has_database_privilege".to_string(), principal.is_admin()));
    }
    Err("unsupported privilege function".into())
}

pub fn reject_unhandled_catalog_projection(sql: &str) -> Result<(), String> {
    let normalized = normalize_for_guard(sql);
    if normalized.contains("pg_catalog.") || normalized.contains("information_schema.") {
        return Err("unsupported catalog projection query".into());
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("hash password failed: {e}"))
}

pub fn verify_password(password: &str, encoded_hash: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

pub fn authenticate_user(conn: &Connection, username: &str, password: &str) -> Result<i64, String> {
    let mut stmt = conn
        .prepare(
            "SELECT user_id, password_hash, password_algo, status \
             FROM rsduck_catalog.rs_user \
             WHERE username = ?",
        )
        .map_err(|e| format!("prepare user authentication failed: {e}"))?;
    let mut rows = stmt
        .query([username])
        .map_err(|e| format!("query user authentication failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read user authentication failed: {e}"))?
    else {
        return Err("invalid username or password".into());
    };

    let user_id: i64 = row
        .get(0)
        .map_err(|e| format!("read authenticated user id failed: {e}"))?;
    let password_hash: String = row
        .get(1)
        .map_err(|e| format!("read password hash failed: {e}"))?;
    let password_algo: String = row
        .get(2)
        .map_err(|e| format!("read password algo failed: {e}"))?;
    let status: String = row
        .get(3)
        .map_err(|e| format!("read user status failed: {e}"))?;

    if status != "active" {
        return Err(format!("user is not active: {username}"));
    }
    if password_algo != "argon2id" {
        return Err(format!(
            "unsupported password algorithm for user {username}: {password_algo}"
        ));
    }
    if !verify_password(password, &password_hash) {
        return Err("invalid username or password".into());
    }

    Ok(user_id)
}

fn principal_for_username(conn: &Connection, username: &str) -> Result<SessionPrincipal, String> {
    let (user_id, status): (i64, String) = conn
        .query_row(
            &format!(
                "SELECT user_id, status FROM rsduck_catalog.rs_user WHERE username = '{}'",
                sql_string(username)
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| format!("unknown user: {username}"))?;
    if status != "active" {
        return Err(format!("user is not active: {username}"));
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.role_name \
                 FROM rsduck_catalog.rs_user_role ur \
                 JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id \
                 WHERE ur.user_id = {user_id} \
                 ORDER BY r.role_name"
        ))
        .map_err(|e| format!("prepare principal roles failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query principal roles failed: {e}"))?;
    let mut roles = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read principal roles failed: {e}"))?
    {
        roles.push(
            row.get(0)
                .map_err(|e| format!("read principal role name failed: {e}"))?,
        );
    }

    Ok(SessionPrincipal {
        user_id,
        username: username.to_string(),
        roles,
    })
}

impl SessionPrincipal {
    fn is_admin(&self) -> bool {
        self.roles.iter().any(|role| role == "admin")
    }

    fn is_operator(&self) -> bool {
        self.roles.iter().any(|role| role == "operator")
    }
}

fn require_system_action(
    conn: &Connection,
    principal: &SessionPrincipal,
    action: &str,
) -> Result<(), String> {
    if principal.is_admin()
        || (principal.is_operator() && matches!(action, "manage_snapshot" | "manage_catalog"))
        || has_explicit_privilege(conn, principal, "system", 0, action)?
    {
        return Ok(());
    }
    Err(format!(
        "permission denied for user {}: system {action}",
        principal.username
    ))
}

fn require_schema_action(
    conn: &Connection,
    principal: &SessionPrincipal,
    schema: &str,
    action: &str,
) -> Result<(), String> {
    let namespace_oid = namespace_oid(conn, schema)?;
    if principal.is_admin()
        || has_explicit_privilege(conn, principal, "schema", namespace_oid, action)?
    {
        return Ok(());
    }
    Err(format!(
        "permission denied for user {}: schema {schema} {action}",
        principal.username
    ))
}

fn require_relation_action(
    conn: &Connection,
    principal: &SessionPrincipal,
    relation: &(String, String),
    action: &str,
) -> Result<(), String> {
    let (schema, relname) = relation;
    let rel_oid = relation_oid(conn, schema, relname)?;
    let namespace_oid = namespace_oid(conn, schema)?;
    if principal.is_admin()
        || has_explicit_privilege(conn, principal, "relation", rel_oid, action)?
        || (action == "read"
            && has_explicit_privilege(conn, principal, "schema", namespace_oid, "read")?)
    {
        return Ok(());
    }
    Err(format!(
        "permission denied for user {}: relation {}.{} {action}",
        principal.username, schema, relname
    ))
}

fn has_relation_action(
    conn: &Connection,
    username: &str,
    relation: &(String, String),
    action: &str,
) -> Result<bool, String> {
    let principal = principal_for_username(conn, username)?;
    let (schema, relname) = relation;
    let rel_oid = relation_oid(conn, schema, relname)?;
    let namespace_oid = namespace_oid(conn, schema)?;
    Ok(principal.is_admin()
        || has_explicit_privilege(conn, &principal, "relation", rel_oid, action)?
        || (action == "read"
            && has_explicit_privilege(conn, &principal, "schema", namespace_oid, "read")?))
}

fn has_schema_action(
    conn: &Connection,
    username: &str,
    schema: &str,
    action: &str,
) -> Result<bool, String> {
    let principal = principal_for_username(conn, username)?;
    let namespace_oid = namespace_oid(conn, schema)?;
    Ok(principal.is_admin()
        || has_explicit_privilege(conn, &principal, "schema", namespace_oid, action)?)
}

fn has_explicit_privilege(
    conn: &Connection,
    principal: &SessionPrincipal,
    object_type: &str,
    object_id: i64,
    action: &str,
) -> Result<bool, String> {
    let role_ids = if principal.roles.is_empty() {
        "NULL".to_string()
    } else {
        let names = principal
            .roles
            .iter()
            .map(|role| format!("'{}'", sql_string(role)))
            .collect::<Vec<_>>()
            .join(",");
        format!("SELECT role_id FROM rsduck_catalog.rs_role WHERE role_name IN ({names})")
    };
    let sql = format!(
        "SELECT COUNT(*) FROM rsduck_catalog.rs_privilege \
         WHERE object_type = '{}' AND object_id = {object_id} AND action = '{}' \
           AND ( \
             (principal_type = 'user' AND principal_id = {}) \
             OR (principal_type = 'role' AND principal_id IN ({role_ids})) \
           )",
        sql_string(object_type),
        sql_string(action),
        principal.user_id
    );
    let count: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|e| format!("check explicit privilege failed: {e}"))?;
    Ok(count > 0)
}

fn execute_catalog_statement(
    conn: &Connection,
    statement: &Statement,
    sql: &str,
    owner_user_id: i64,
) -> Result<Option<usize>, String> {
    match statement {
        Statement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => create_schema(conn, schema_name, *if_not_exists, owner_user_id).map(Some),
        Statement::CreateUser(create_user) => {
            create_user_account(conn, create_user, sql, owner_user_id).map(Some)
        }
        Statement::AlterUser(alter_user) => {
            alter_user_account(conn, alter_user, sql, owner_user_id).map(Some)
        }
        Statement::CreateTable(create_table) => {
            create_table_relation(conn, create_table, sql, owner_user_id).map(Some)
        }
        Statement::CreateView(create_view) => {
            create_view_relation(conn, create_view, sql, owner_user_id).map(Some)
        }
        Statement::CreateIndex(create_index) => {
            create_index_relation(conn, create_index, sql, owner_user_id).map(Some)
        }
        Statement::AlterTable(alter_table) => {
            alter_table_relation(conn, alter_table, sql, owner_user_id).map(Some)
        }
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            table,
            ..
        } => drop_objects(
            conn,
            *object_type,
            *if_exists,
            names,
            *cascade,
            table.as_ref(),
            sql,
        )
        .map(Some),
        Statement::Comment {
            object_type,
            object_name,
            comment,
            if_exists,
        } => comment_object(
            conn,
            *object_type,
            object_name,
            comment.as_deref(),
            *if_exists,
            sql,
        )
        .map(Some),
        Statement::Insert(insert) => {
            insert_partitioned_relation(conn, insert, sql).map(|affected| {
                if affected == 0 {
                    None
                } else {
                    Some(affected)
                }
            })
        }
        Statement::Grant(grant) => grant_privileges(conn, grant, sql, owner_user_id).map(Some),
        Statement::Revoke(revoke) => revoke_privileges(conn, revoke, sql).map(Some),
        Statement::AlterSchema(_) | Statement::AlterIndex { .. } | Statement::AlterView { .. } => {
            Err(format!(
                "catalog mutation is not implemented for this statement: {}",
                statement_command(statement)
            ))
        }
        _ => {
            guard_external_sql(sql)?;
            Ok(None)
        }
    }
}

fn create_schema(
    conn: &Connection,
    schema_name: &SchemaName,
    if_not_exists: bool,
    owner_user_id: i64,
) -> Result<usize, String> {
    let schema = schema_name_value(schema_name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if namespace_exists(conn, &schema)? {
            if if_not_exists {
                return Ok(0);
            }
            return Err(format!("schema already exists: {schema}"));
        }

        let ns_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_schema", ns_oid, &schema)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_namespace(oid, nspname, nspowner, nspacl) \
                 VALUES ({ns_oid}, '{}', {owner_user_id}, '')",
                sql_string(&schema)
            ),
            [],
        )
        .map_err(|e| format!("write pg_namespace failed: {e}"))?;
        conn.execute(&format!("CREATE SCHEMA {}", quote_ident(&schema)), [])
            .map_err(|e| format!("execute DuckDB CREATE SCHEMA failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn create_user_account(
    conn: &Connection,
    create_user: &CreateUser,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_user.or_replace {
        return Err("CREATE OR REPLACE USER is not supported".into());
    }
    let username = create_user.name.value.trim();
    validate_username(username)?;
    let password = quoted_literals(sql)
        .into_iter()
        .next()
        .ok_or_else(|| "CREATE USER requires PASSWORD '<password>'".to_string())?;
    let password_hash = hash_password(&password)?;

    run_catalog_tx(conn, || {
        if user_exists(conn, username)? {
            if create_user.if_not_exists {
                return Ok(0);
            }
            return Err(format!("user already exists: {username}"));
        }
        let user_id = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_user", user_id, sql)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, status, is_builtin, created_at, updated_at, last_login_at) \
                 VALUES ({user_id}, '{}', '{}', 'argon2id', 'active', FALSE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)",
                sql_string(username),
                sql_string(&password_hash)
            ),
            [],
        )
        .map_err(|e| format!("write rs_user failed: {e}"))?;
        let reader_role = role_id_by_name(conn, "reader")?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at) \
                 VALUES ({user_id}, {reader_role}, {owner_user_id}, CURRENT_TIMESTAMP)"
            ),
            [],
        )
        .map_err(|e| format!("write default user role failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

fn alter_user_account(
    conn: &Connection,
    alter_user: &AlterUser,
    sql: &str,
    _owner_user_id: i64,
) -> Result<usize, String> {
    if alter_user.rename_to.is_some()
        || alter_user.reset_password
        || alter_user.abort_all_queries
        || alter_user.add_role_delegation.is_some()
        || alter_user.remove_role_delegation.is_some()
        || alter_user.enroll_mfa
        || alter_user.set_default_mfa_method.is_some()
        || alter_user.remove_mfa_method.is_some()
        || alter_user.modify_mfa_method.is_some()
        || alter_user.add_mfa_method_otp.is_some()
        || alter_user.set_policy.is_some()
        || alter_user.unset_policy.is_some()
        || !alter_user.set_tag.options.is_empty()
        || !alter_user.unset_tag.is_empty()
        || !alter_user.set_props.options.is_empty()
        || !alter_user.unset_props.is_empty()
    {
        return Err("ALTER USER only supports PASSWORD changes".into());
    }

    let username = alter_user.name.value.trim();
    validate_username(username)?;
    let Some(password) = &alter_user.password else {
        return Err("ALTER USER requires PASSWORD '<password>'".into());
    };
    let password = password
        .password
        .as_ref()
        .ok_or_else(|| "ALTER USER PASSWORD NULL is not supported".to_string())?;
    let password_hash = hash_password(password)?;

    run_catalog_tx(conn, || {
        let Some(user_id) = user_id_by_name_opt(conn, username)? else {
            if alter_user.if_exists {
                return Ok(0);
            }
            return Err(format!("user does not exist: {username}"));
        };
        let journal_id = insert_journal(conn, "alter_user_password", user_id, sql)?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_user \
                 SET password_hash = '{}', password_algo = 'argon2id', updated_at = CURRENT_TIMESTAMP \
                 WHERE user_id = {user_id}",
                sql_string(&password_hash)
            ),
            [],
        )
        .map_err(|e| format!("update user password failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

fn create_table_relation(
    conn: &Connection,
    create_table: &CreateTable,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_table.partition_by.is_some() {
        return Err("managed range partitioned table mutation is not implemented yet".into());
    }
    if create_table.query.is_some() {
        return Err("CREATE TABLE AS is not supported by catalog mutation yet".into());
    }
    if create_table.temporary {
        return Err("temporary table is not supported by rsduck catalog".into());
    }

    let (schema, table) = relation_name(&create_table.name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &table)? {
            if create_table.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{table}"));
        }

        ensure_user_schema_exists(conn, &schema)?;
        let rel_oid = allocate_oid(conn)?;
        let type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_table", rel_oid, sql)?;

        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB CREATE TABLE failed: {e}"))?;

        let columns = load_duckdb_columns(conn, &schema, &table)?;
        insert_relation_rows(
            conn,
            rel_oid,
            type_oid,
            &schema,
            &table,
            "r",
            "ordinary",
            "user",
            "",
            &columns,
            owner_user_id,
        )?;
        insert_create_table_constraints(conn, rel_oid, &schema, &table, &columns, create_table)?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn create_range_partitioned_table(
    conn: &Connection,
    partitioned: &ManagedPartitionCreate,
    owner_user_id: i64,
) -> Result<usize, String> {
    let (statement, _) = parse_one_statement(&partitioned.base_sql)?;
    let Statement::CreateTable(create_table) = statement else {
        return Err("managed partitioned table base DDL must be CREATE TABLE".into());
    };
    if create_table.query.is_some() {
        return Err("CREATE TABLE AS is not supported by managed partitioned table".into());
    }
    if create_table.temporary {
        return Err("temporary managed partitioned table is not supported".into());
    }
    if !create_table.constraints.is_empty() {
        return Err(
            "table constraints on managed partitioned table are not implemented yet".into(),
        );
    }

    let (schema, table) = relation_name(&create_table.name)?;
    reject_reserved_schema(&schema)?;
    let (partition_key_type, _) = validate_partition_key(
        &create_table,
        &partitioned.partition_key,
        &partitioned.partition_unit,
    )?;
    let null_partition = physical_partition_name(&table, "_null");
    let view_sql =
        partition_entrypoint_sql(&schema, &table, &[("rsduck_internal", &null_partition)]);

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &table)? {
            if create_table.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{table}"));
        }
        if relation_exists(conn, "rsduck_internal", &null_partition)? {
            return Err(format!(
                "managed physical partition relation already exists: rsduck_internal.{null_partition}"
            ));
        }

        ensure_user_schema_exists(conn, &schema)?;
        let parent_oid = allocate_oid(conn)?;
        let parent_type_oid = allocate_oid(conn)?;
        let child_oid = allocate_oid(conn)?;
        let child_type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(
            conn,
            "create_range_partitioned_table",
            parent_oid,
            &partitioned.base_sql,
        )?;

        let create_null_sql = physical_partition_create_sql(&null_partition, &create_table);
        conn.execute(&create_null_sql, [])
            .map_err(|e| format!("execute DuckDB CREATE null partition failed: {e}"))?;
        conn.execute(&view_sql, [])
            .map_err(|e| format!("execute DuckDB CREATE partition entrypoint failed: {e}"))?;

        let columns = load_duckdb_columns(conn, "rsduck_internal", &null_partition)?;
        insert_relation_rows(
            conn,
            parent_oid,
            parent_type_oid,
            &schema,
            &table,
            "p",
            "range_partitioned_table",
            "user",
            &view_sql,
            &columns,
            owner_user_id,
        )?;
        update_partition_relation_ext(
            conn,
            parent_oid,
            &partitioned.partition_key,
            &partition_key_type,
            &partitioned.partition_unit,
            partitioned.retention_count,
            &view_sql,
        )?;

        insert_relation_rows(
            conn,
            child_oid,
            child_type_oid,
            "rsduck_internal",
            &null_partition,
            "r",
            "physical_partition",
            "internal",
            "",
            &columns,
            owner_user_id,
        )?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class \
                 SET relispartition = TRUE, relpartbound = '_null' \
                 WHERE oid = {child_oid}"
            ),
            [],
        )
        .map_err(|e| format!("mark null partition pg_class failed: {e}"))?;
        update_partition_relation_ext(
            conn,
            child_oid,
            &partitioned.partition_key,
            &partition_key_type,
            "null",
            0,
            "",
        )?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_partition(parent_relid, child_relid, partition_value, \
                 partition_unit, lower_bound, upper_bound, is_null_partition, status, row_count, min_ts, \
                 max_ts, checksum, created_at, activated_at, dropped_at, error_message) \
                 VALUES ({parent_oid}, {child_oid}, '_null', 'null', NULL, NULL, TRUE, 'active', 0, \
                 NULL, NULL, '', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, '')"
            ),
            [],
        )
        .map_err(|e| format!("write null partition metadata failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({PG_CLASS_CLASSOID}, {parent_oid}, 0, {PG_CLASS_CLASSOID}, {child_oid}, 0, 'n')"
            ),
            [],
        )
        .map_err(|e| format!("write partition dependency failed: {e}"))?;

        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn insert_partitioned_relation(
    conn: &Connection,
    insert: &Insert,
    sql: &str,
) -> Result<usize, String> {
    let TableObject::TableName(table_name) = &insert.table else {
        return Ok(0);
    };
    let (schema, table) = relation_name(table_name)?;
    reject_reserved_schema(&schema)?;
    let Some(relation) = partitioned_relation(conn, &schema, &table)? else {
        return Ok(0);
    };
    if insert.source.is_none() {
        return Err("INSERT into managed partitioned table requires VALUES".into());
    }
    if !insert.assignments.is_empty()
        || insert.returning.is_some()
        || insert.on.is_some()
        || insert.overwrite
        || insert.partitioned.is_some()
        || insert.format_clause.is_some()
    {
        return Err("unsupported INSERT form for managed partitioned table".into());
    }

    let target_columns = insert_target_columns(insert, &relation)?;
    let partition_key_idx = target_columns
        .iter()
        .position(|column| column.eq_ignore_ascii_case(&relation.partition_key));
    let source = insert.source.as_ref().expect("source checked");
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return Err("INSERT into managed partitioned table supports VALUES only".into());
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err("INSERT into managed partitioned table supports VALUES only".into());
    };

    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "insert_partitioned_rows", relation.oid, sql)?;
        let mut groups: Vec<(String, Option<NaiveDateTime>, Vec<Vec<String>>)> = Vec::new();
        for row in &values.rows {
            if row.content.len() != target_columns.len() {
                return Err(format!(
                    "INSERT column count mismatch: target={}, row={}",
                    target_columns.len(),
                    row.content.len()
                ));
            }
            let route = if let Some(idx) = partition_key_idx {
                partition_route_for_expr(
                    &row.content[idx],
                    &relation.partition_key_type,
                    &relation.partition_unit,
                )
            } else {
                PartitionRoute {
                    partition_value: "_null".to_string(),
                    route_ts: None,
                }
            };
            let mut exprs = row
                .content
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if route.partition_value == "_null" {
                if let Some(idx) = partition_key_idx {
                    exprs[idx] = "NULL".to_string();
                }
            }

            if let Some((_, existing_ts, rows)) = groups
                .iter_mut()
                .find(|(partition_value, _, _)| partition_value == &route.partition_value)
            {
                if existing_ts.is_none() {
                    *existing_ts = route.route_ts;
                }
                rows.push(exprs);
            } else {
                groups.push((route.partition_value, route.route_ts, vec![exprs]));
            }
        }

        let mut affected = 0usize;
        for (partition_value, route_ts, rows) in groups {
            let child_relname = ensure_active_partition(conn, &relation, &partition_value)?;
            let values_sql = rows
                .iter()
                .map(|row| format!("({})", row.join(", ")))
                .collect::<Vec<_>>()
                .join(", ");
            let insert_sql = format!(
                "INSERT INTO {} ({}) VALUES {values_sql}",
                quote_qualified("rsduck_internal", &child_relname),
                target_columns
                    .iter()
                    .map(|column| quote_ident(column))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            conn.execute(&insert_sql, [])
                .map_err(|e| format!("insert partition rows failed: {e}"))?;
            update_partition_stats(
                conn,
                relation.oid,
                &partition_value,
                rows.len() as i64,
                route_ts,
            )?;
            affected += rows.len();
        }

        refresh_partition_entrypoint(conn, relation.oid, &relation.schema, &relation.name)?;
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

fn partitioned_relation(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<Option<PartitionedRelation>, String> {
    let Some(meta) = find_relation_meta(conn, schema, table)? else {
        return Ok(None);
    };
    if meta.relkind != "p" {
        return Ok(None);
    }
    let (partition_key, partition_key_type, partition_unit): (String, String, String) = conn
        .query_row(
            &format!(
                "SELECT partition_key, partition_key_type, partition_unit \
                 FROM rsduck_catalog.rs_relation_ext \
                 WHERE relid = {} AND managed_kind = 'range_partitioned_table'",
                meta.oid
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| format!("read partitioned relation metadata failed: {e}"))?;
    Ok(Some(PartitionedRelation {
        oid: meta.oid,
        schema: schema.to_string(),
        name: table.to_string(),
        partition_key,
        partition_key_type,
        partition_unit,
        columns: catalog_columns(conn, meta.oid)?,
    }))
}

fn insert_target_columns(
    insert: &Insert,
    relation: &PartitionedRelation,
) -> Result<Vec<String>, String> {
    if insert.columns.is_empty() {
        return Ok(relation
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect());
    }
    insert
        .columns
        .iter()
        .map(single_name_part)
        .map(|result| {
            let column = result?;
            if relation
                .columns
                .iter()
                .any(|catalog_column| catalog_column.name.eq_ignore_ascii_case(&column))
            {
                Ok(column)
            } else {
                Err(format!("INSERT references unknown column: {column}"))
            }
        })
        .collect()
}

fn partition_route_for_expr(
    expr: &Expr,
    partition_key_type: &str,
    partition_unit: &str,
) -> PartitionRoute {
    let Some(dt) = partition_datetime_from_expr(expr, partition_key_type) else {
        return PartitionRoute {
            partition_value: "_null".to_string(),
            route_ts: None,
        };
    };
    PartitionRoute {
        partition_value: partition_value_for_datetime(dt, partition_unit),
        route_ts: Some(dt),
    }
}

fn partition_datetime_from_expr(expr: &Expr, partition_key_type: &str) -> Option<NaiveDateTime> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Null => None,
            Value::SingleQuotedString(value)
            | Value::TripleSingleQuotedString(value)
            | Value::EscapedStringLiteral(value)
            | Value::UnicodeStringLiteral(value) => {
                parse_partition_datetime(value, partition_key_type)
            }
            _ => None,
        },
        Expr::TypedString(value) => match &value.value.value {
            Value::SingleQuotedString(text)
            | Value::TripleSingleQuotedString(text)
            | Value::EscapedStringLiteral(text)
            | Value::UnicodeStringLiteral(text) => {
                parse_partition_datetime(text, partition_key_type)
            }
            _ => None,
        },
        _ => None,
    }
}

fn parse_partition_datetime(value: &str, partition_key_type: &str) -> Option<NaiveDateTime> {
    let value = value.trim();
    if partition_key_type == "date" {
        return NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0));
    }
    parse_timestamp_literal(value)
}

fn parse_timestamp_literal(value: &str) -> Option<NaiveDateTime> {
    for pattern in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, pattern) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
}

fn partition_value_for_datetime(dt: NaiveDateTime, partition_unit: &str) -> String {
    match partition_unit {
        "hour" => format!(
            "{:04}{:02}{:02}{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ),
        "day" => format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()),
        "month" => format!("{:04}{:02}", dt.year(), dt.month()),
        "year" => format!("{:04}", dt.year()),
        _ => "_null".to_string(),
    }
}

fn create_view_relation(
    conn: &Connection,
    create_view: &CreateView,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_view.or_replace {
        return Err("CREATE OR REPLACE VIEW is not supported by catalog mutation yet".into());
    }
    if create_view.temporary {
        return Err("temporary view is not supported by rsduck catalog".into());
    }

    let (schema, view) = relation_name(&create_view.name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &view)? {
            if create_view.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{view}"));
        }

        ensure_user_schema_exists(conn, &schema)?;
        let rel_oid = allocate_oid(conn)?;
        let type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_view", rel_oid, sql)?;

        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB CREATE VIEW failed: {e}"))?;

        let columns = load_duckdb_columns(conn, &schema, &view)?;
        insert_relation_rows(
            conn,
            rel_oid,
            type_oid,
            &schema,
            &view,
            "v",
            "generated_view",
            "user",
            &create_view.query.to_string(),
            &columns,
            owner_user_id,
        )?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn create_index_relation(
    conn: &Connection,
    create_index: &CreateIndex,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_index.name.is_none() {
        return Err("CREATE INDEX requires an explicit index name".into());
    }
    if create_index.predicate.is_some() {
        return Err("partial index is not supported by rsduck catalog".into());
    }
    if !create_index.include.is_empty() {
        return Err("index INCLUDE columns are not supported by rsduck catalog".into());
    }

    let (table_schema, table_name) = relation_name(&create_index.table_name)?;
    reject_reserved_schema(&table_schema)?;

    let index_name = create_index.name.as_ref().expect("index name checked");
    let (index_schema, index_relname) = relation_name_with_default(index_name, &table_schema)?;
    if index_schema != table_schema {
        return Err("index schema must match table schema".into());
    }

    let index_column_names = simple_index_column_names(&create_index.columns)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &index_schema, &index_relname)? {
            if create_index.if_not_exists {
                return Ok(0);
            }
            return Err(format!(
                "relation already exists: {index_schema}.{index_relname}"
            ));
        }

        let table_oid = relation_oid(conn, &table_schema, &table_name)?;
        let table_kind = relation_kind(conn, table_oid)?;
        if table_kind != "r" {
            return Err(format!(
                "CREATE INDEX only supports ordinary tables, got relkind={table_kind}"
            ));
        }

        let table_columns = catalog_columns(conn, table_oid)?;
        let mut indkey = Vec::with_capacity(index_column_names.len());
        for column_name in &index_column_names {
            let attnum = table_columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(column_name))
                .map(|column| column.attnum)
                .ok_or_else(|| format!("index references unknown column: {column_name}"))?;
            indkey.push(attnum.to_string());
        }

        let index_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_index", index_oid, sql)?;
        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB CREATE INDEX failed: {e}"))?;

        let namespace_oid = namespace_oid(conn, &index_schema)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_class(oid, relname, relnamespace, reltype, relowner, \
                 relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message) \
                 VALUES ({index_oid}, '{}', {namespace_oid}, 0, {owner_user_id}, 'i', 'p', {}, 0, FALSE, FALSE, '', '', 'active', '')",
                sql_string(&index_relname),
                index_column_names.len()
            ),
            [],
        )
        .map_err(|e| format!("write index pg_class failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_index(indexrelid, indrelid, indnatts, indnkeyatts, \
                 indisunique, indisprimary, indisvalid, indkey, indexprs, indpred) \
                 VALUES ({index_oid}, {table_oid}, {}, {}, {}, FALSE, TRUE, '{}', '', '')",
                index_column_names.len(),
                index_column_names.len(),
                sql_bool(create_index.unique),
                sql_string(&indkey.join(","))
            ),
            [],
        )
        .map_err(|e| format!("write pg_index failed: {e}"))?;

        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class SET relhasindex = TRUE WHERE oid = {table_oid}"
            ),
            [],
        )
        .map_err(|e| format!("update table relhasindex failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES (1259, {index_oid}, 0, 1259, {table_oid}, 0, 'n')"
            ),
            [],
        )
        .map_err(|e| format!("write index dependency failed: {e}"))?;

        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn alter_table_relation(
    conn: &Connection,
    alter_table: &AlterTable,
    sql: &str,
    _owner_user_id: i64,
) -> Result<usize, String> {
    let (schema, table) = relation_name(&alter_table.name)?;
    reject_reserved_schema(&schema)?;
    if alter_table.operations.len() != 1 {
        return Err("ALTER TABLE currently supports exactly one operation".into());
    }

    let AlterTableOperation::AddColumn {
        if_not_exists,
        column_def,
        column_position,
        ..
    } = &alter_table.operations[0]
    else {
        return Err("only ALTER TABLE ADD COLUMN is implemented by rsduck catalog".into());
    };
    if column_position.is_some() {
        return Err("ALTER TABLE ADD COLUMN position is not supported by rsduck catalog".into());
    }

    run_catalog_tx(conn, || {
        let rel_oid = relation_oid(conn, &schema, &table)?;
        let relkind = relation_kind(conn, rel_oid)?;
        if column_exists(conn, rel_oid, &column_def.name.value)? {
            if *if_not_exists {
                return Ok(0);
            }
            return Err(format!(
                "column already exists: {}.{}.{}",
                schema, table, column_def.name
            ));
        }

        let journal_id = insert_journal(conn, "alter_table_add_column", rel_oid, sql)?;
        if relkind == "p" {
            alter_partitioned_table_add_column(
                conn,
                rel_oid,
                &schema,
                &table,
                &column_def.to_string(),
            )?;
            finish_journal(conn, journal_id)?;
            return Ok(0);
        }
        if relkind != "r" {
            return Err(format!(
                "ALTER TABLE ADD COLUMN only supports ordinary or partitioned tables, got relkind={relkind}"
            ));
        }
        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB ALTER TABLE ADD COLUMN failed: {e}"))?;
        let physical_columns = load_duckdb_columns(conn, &schema, &table)?;
        let column = physical_columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&column_def.name.value))
            .ok_or_else(|| format!("DuckDB did not expose added column: {}", column_def.name))?;
        insert_attribute_row(conn, rel_oid, column)?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class SET relnatts = relnatts + 1 WHERE oid = {rel_oid}"
            ),
            [],
        )
        .map_err(|e| format!("update relnatts failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn alter_partitioned_table_add_column(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    column_def_sql: &str,
) -> Result<(), String> {
    let children = active_partition_children(conn, parent_oid)?;
    if children.is_empty() {
        return Err("partitioned table has no active physical partitions".into());
    }

    let mut parent_column: Option<CatalogColumn> = None;
    for child in &children {
        conn.execute(
            &format!(
                "ALTER TABLE {} ADD COLUMN {column_def_sql}",
                quote_qualified(&child.schema, &child.relname)
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB ALTER TABLE ADD COLUMN on partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;

        let physical_columns = load_duckdb_columns(conn, &child.schema, &child.relname)?;
        let column = physical_columns
            .last()
            .ok_or_else(|| {
                format!(
                    "DuckDB partition has no columns: {}.{}",
                    child.schema, child.relname
                )
            })?
            .clone();
        insert_attribute_row(conn, child.child_oid, &column)?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class SET relnatts = relnatts + 1 WHERE oid = {}",
                child.child_oid
            ),
            [],
        )
        .map_err(|e| format!("update child relnatts failed: {e}"))?;

        if parent_column.is_none() {
            parent_column = Some(column);
        }
    }

    let parent_column = parent_column
        .ok_or_else(|| "partitioned table has no active physical partitions".to_string())?;
    insert_attribute_row(conn, parent_oid, &parent_column)?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.pg_class SET relnatts = relnatts + 1 WHERE oid = {parent_oid}"
        ),
        [],
    )
    .map_err(|e| format!("update parent relnatts failed: {e}"))?;
    refresh_partition_entrypoint(conn, parent_oid, schema, table)
}

fn drop_objects(
    conn: &Connection,
    object_type: ObjectType,
    if_exists: bool,
    names: &[ObjectName],
    cascade: bool,
    table: Option<&ObjectName>,
    sql: &str,
) -> Result<usize, String> {
    if names.is_empty() {
        return Err("DROP requires at least one object name".into());
    }
    if object_type == ObjectType::User {
        return drop_user_accounts(conn, if_exists, names, sql);
    }

    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in names {
            let (schema, relname) = match (object_type, table) {
                (ObjectType::Index, Some(table_name)) => {
                    let (table_schema, _) = relation_name(table_name)?;
                    relation_name_with_default(name, &table_schema)?
                }
                _ => relation_name(name)?,
            };
            reject_reserved_schema(&schema)?;
            let Some(meta) = find_relation_meta(conn, &schema, &relname)? else {
                if if_exists {
                    continue;
                }
                return Err(format!("relation does not exist: {schema}.{relname}"));
            };
            ensure_drop_type(object_type, &meta)?;
            if meta.relispartition {
                return Err("cannot directly drop managed physical partition".into());
            }

            let journal_id = insert_journal(conn, "drop_relation", meta.oid, sql)?;
            drop_relation_dependencies(conn, &meta, cascade)?;
            if meta.relkind == "p" {
                drop_partitioned_relation(conn, &meta, &schema, &relname)?;
            } else {
                execute_physical_drop(conn, object_type, &schema, &relname)?;
                delete_relation_catalog(conn, &meta)?;
            }
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

fn drop_user_accounts(
    conn: &Connection,
    if_exists: bool,
    names: &[ObjectName],
    sql: &str,
) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in names {
            let username = single_name_part(name)?;
            if username.eq_ignore_ascii_case("admin") {
                return Err("default admin user cannot be dropped".into());
            }
            let Some(user_id) = user_id_by_name_opt(conn, &username)? else {
                if if_exists {
                    continue;
                }
                return Err(format!("user does not exist: {username}"));
            };
            let journal_id = insert_journal(conn, "drop_user", user_id, sql)?;
            for statement in [
                format!("DELETE FROM rsduck_catalog.rs_user_role WHERE user_id = {user_id}"),
                format!(
                    "DELETE FROM rsduck_catalog.rs_privilege \
                     WHERE principal_type = 'user' AND principal_id = {user_id}"
                ),
                format!("DELETE FROM rsduck_catalog.rs_user WHERE user_id = {user_id}"),
            ] {
                conn.execute(&statement, [])
                    .map_err(|e| format!("drop user catalog rows failed: {e}"))?;
            }
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

fn drop_partitioned_relation(
    conn: &Connection,
    meta: &RelationMeta,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let children = partition_child_metas(conn, meta.oid)?;
    conn.execute(
        &format!("DROP VIEW {}", quote_qualified(schema, relname)),
        [],
    )
    .map_err(|e| format!("execute DuckDB DROP partition entrypoint failed: {e}"))?;
    for child in children {
        conn.execute(
            &format!(
                "DROP TABLE {}",
                quote_qualified(&child.schema, &child.relname)
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB DROP physical partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;
        delete_relation_catalog(conn, &child.meta)?;
    }
    delete_relation_catalog(conn, meta)
}

#[derive(Debug)]
struct PartitionChildMeta {
    meta: RelationMeta,
    schema: String,
    relname: String,
}

fn partition_child_metas(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<PartitionChildMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.oid, c.reltype, c.relkind, c.relispartition, n.nspname, c.relname \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
             ORDER BY p.is_null_partition, p.partition_value"
        ))
        .map_err(|e| format!("prepare partition child lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition child lookup failed: {e}"))?;
    let mut children = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition child lookup failed: {e}"))?
    {
        children.push(PartitionChildMeta {
            meta: RelationMeta {
                oid: row
                    .get(0)
                    .map_err(|e| format!("read partition child oid failed: {e}"))?,
                reltype: row
                    .get(1)
                    .map_err(|e| format!("read partition child reltype failed: {e}"))?,
                relkind: row
                    .get(2)
                    .map_err(|e| format!("read partition child relkind failed: {e}"))?,
                relispartition: row
                    .get(3)
                    .map_err(|e| format!("read partition child flag failed: {e}"))?,
            },
            schema: row
                .get(4)
                .map_err(|e| format!("read partition child schema failed: {e}"))?,
            relname: row
                .get(5)
                .map_err(|e| format!("read partition child name failed: {e}"))?,
        });
    }
    Ok(children)
}

fn grant_privileges(
    conn: &Connection,
    grant: &Grant,
    sql: &str,
    grantor_id: i64,
) -> Result<usize, String> {
    let targets = grant_targets(conn, grant.objects.as_ref())?;
    let actions = privilege_actions(&grant.privileges, &targets)?;
    let principals = grant_principals(conn, &grant.grantees)?;

    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "grant_privilege", 0, sql)?;
        let mut affected = 0;
        for (principal_type, principal_id) in &principals {
            for (object_type, object_id) in &targets {
                for action in &actions {
                    affected += upsert_privilege(
                        conn,
                        principal_type,
                        *principal_id,
                        object_type,
                        *object_id,
                        action,
                        grantor_id,
                    )?;
                }
            }
        }
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

fn revoke_privileges(conn: &Connection, revoke: &Revoke, sql: &str) -> Result<usize, String> {
    let targets = grant_targets(conn, revoke.objects.as_ref())?;
    let actions = privilege_actions(&revoke.privileges, &targets)?;
    let principals = grant_principals(conn, &revoke.grantees)?;

    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "revoke_privilege", 0, sql)?;
        let mut affected = 0;
        for (principal_type, principal_id) in &principals {
            for (object_type, object_id) in &targets {
                for action in &actions {
                    affected += conn
                        .execute(
                            &format!(
                                "DELETE FROM rsduck_catalog.rs_privilege \
                                 WHERE principal_type = '{}' AND principal_id = {} \
                                   AND object_type = '{}' AND object_id = {} AND action = '{}'",
                                sql_string(principal_type),
                                principal_id,
                                sql_string(object_type),
                                object_id,
                                sql_string(action)
                            ),
                            [],
                        )
                        .map_err(|e| format!("delete privilege failed: {e}"))?;
                }
            }
        }
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

fn grant_targets(
    conn: &Connection,
    objects: Option<&GrantObjects>,
) -> Result<Vec<(String, i64)>, String> {
    let Some(objects) = objects else {
        return Ok(vec![("system".to_string(), 0)]);
    };
    match objects {
        GrantObjects::Tables(names) | GrantObjects::Views(names) => names
            .iter()
            .map(|name| {
                let (schema, relation) = relation_name(name)?;
                let relid = relation_oid(conn, &schema, &relation)?;
                Ok(("relation".to_string(), relid))
            })
            .collect(),
        GrantObjects::Schemas(names) => names
            .iter()
            .map(|name| {
                let schema = single_name_part(name)?;
                Ok(("schema".to_string(), namespace_oid(conn, &schema)?))
            })
            .collect(),
        GrantObjects::Databases(_) => Ok(vec![("system".to_string(), 0)]),
        _ => Err(format!("GRANT target is not supported: {objects}")),
    }
}

fn grant_principals(
    conn: &Connection,
    grantees: &[sqlparser::ast::Grantee],
) -> Result<Vec<(String, i64)>, String> {
    if grantees.is_empty() {
        return Err("GRANT/REVOKE requires at least one grantee".into());
    }
    grantees
        .iter()
        .map(|grantee| {
            let name = match &grantee.name {
                Some(GranteeName::ObjectName(name)) => single_name_part(name)?,
                Some(GranteeName::UserHost { user, .. }) => user.value.clone(),
                None => return Err("grantee name is required".into()),
            };
            match grantee.grantee_type {
                GranteesType::Role => Ok(("role".to_string(), role_id_by_name(conn, &name)?)),
                GranteesType::None | GranteesType::User => {
                    Ok(("user".to_string(), user_id_by_name(conn, &name)?))
                }
                _ => Err(format!("unsupported grantee type: {}", grantee)),
            }
        })
        .collect()
}

fn privilege_actions(
    privileges: &Privileges,
    targets: &[(String, i64)],
) -> Result<Vec<String>, String> {
    let object_type = targets
        .first()
        .map(|(object_type, _)| object_type.as_str())
        .unwrap_or("system");
    let mut actions = Vec::new();
    match privileges {
        Privileges::All { .. } => match object_type {
            "relation" => actions.extend(["read", "write", "ddl"].map(str::to_string)),
            "schema" => actions.extend(["read", "ddl"].map(str::to_string)),
            "system" => actions
                .extend(["manage_snapshot", "manage_catalog", "manage_user"].map(str::to_string)),
            _ => return Err(format!("unsupported privilege object type: {object_type}")),
        },
        Privileges::Actions(items) => {
            for item in items {
                let action = match (object_type, item) {
                    (_, Action::Select { .. } | Action::Read | Action::Usage) => "read",
                    (
                        "relation",
                        Action::Insert { .. } | Action::Update { .. } | Action::Delete,
                    ) => "write",
                    ("relation", Action::Create { .. } | Action::Drop | Action::Ownership) => "ddl",
                    ("schema", Action::Create { .. } | Action::Drop | Action::Ownership) => "ddl",
                    ("system", Action::Create { .. } | Action::Ownership) => "manage_user",
                    _ => {
                        return Err(format!(
                            "unsupported privilege action for {object_type}: {item}"
                        ))
                    }
                };
                if !actions.iter().any(|existing| existing == action) {
                    actions.push(action.to_string());
                }
            }
        }
    }
    if actions.is_empty() {
        return Err("GRANT/REVOKE produced no supported privilege actions".into());
    }
    Ok(actions)
}

fn upsert_privilege(
    conn: &Connection,
    principal_type: &str,
    principal_id: i64,
    object_type: &str,
    object_id: i64,
    action: &str,
    grantor_id: i64,
) -> Result<usize, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_privilege \
                 WHERE principal_type = '{}' AND principal_id = {} \
                   AND object_type = '{}' AND object_id = {} AND action = '{}'",
                sql_string(principal_type),
                principal_id,
                sql_string(object_type),
                object_id,
                sql_string(action)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check existing privilege failed: {e}"))?;
    if count > 0 {
        return Ok(0);
    }
    let privilege_id = allocate_oid(conn)?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_privilege(privilege_id, principal_type, principal_id, object_type, object_id, action, granted_by, created_at) \
             VALUES ({privilege_id}, '{}', {principal_id}, '{}', {object_id}, '{}', {grantor_id}, CURRENT_TIMESTAMP)",
            sql_string(principal_type),
            sql_string(object_type),
            sql_string(action)
        ),
        [],
    )
    .map_err(|e| format!("insert privilege failed: {e}"))?;
    Ok(1)
}

fn comment_object(
    conn: &Connection,
    object_type: CommentObject,
    object_name: &ObjectName,
    comment: Option<&str>,
    if_exists: bool,
    sql: &str,
) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let (objoid, classoid, objsubid) = match object_type {
            CommentObject::Schema => {
                let schema = single_name_part(object_name)?;
                match namespace_oid(conn, &schema) {
                    Ok(oid) => (oid, PG_NAMESPACE_CLASSOID, 0),
                    Err(_) if if_exists => return Ok(0),
                    Err(err) => return Err(err),
                }
            }
            CommentObject::Table | CommentObject::View | CommentObject::Index => {
                let (schema, relname) = relation_name(object_name)?;
                match find_relation_meta(conn, &schema, &relname)? {
                    Some(meta) => (meta.oid, PG_CLASS_CLASSOID, 0),
                    None if if_exists => return Ok(0),
                    None => return Err(format!("relation does not exist: {schema}.{relname}")),
                }
            }
            CommentObject::Column => {
                let (schema, relname, column) = column_comment_target(object_name)?;
                let Some(meta) = find_relation_meta(conn, &schema, &relname)? else {
                    if if_exists {
                        return Ok(0);
                    }
                    return Err(format!("relation does not exist: {schema}.{relname}"));
                };
                let Some(attnum) = column_attnum(conn, meta.oid, &column)? else {
                    if if_exists {
                        return Ok(0);
                    }
                    return Err(format!(
                        "column does not exist: {schema}.{relname}.{column}"
                    ));
                };
                (meta.oid, PG_CLASS_CLASSOID, attnum)
            }
            _ => return Err(format!("COMMENT ON {object_type} is not supported")),
        };

        let journal_id = insert_journal(conn, "comment_object", objoid, sql)?;
        conn.execute(
            &format!(
                "DELETE FROM rsduck_catalog.pg_description \
                 WHERE objoid = {objoid} AND classoid = {classoid} AND objsubid = {objsubid}"
            ),
            [],
        )
        .map_err(|e| format!("delete previous comment failed: {e}"))?;
        if let Some(comment) = comment {
            conn.execute(
                &format!(
                    "INSERT INTO rsduck_catalog.pg_description(objoid, classoid, objsubid, description) \
                     VALUES ({objoid}, {classoid}, {objsubid}, '{}')",
                    sql_string(comment)
                ),
                [],
            )
            .map_err(|e| format!("write object comment failed: {e}"))?;
        }
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn insert_relation_rows(
    conn: &Connection,
    rel_oid: i64,
    type_oid: i64,
    schema: &str,
    relation: &str,
    relkind: &str,
    managed_kind: &str,
    visibility: &str,
    generated_sql: &str,
    columns: &[CatalogColumn],
    owner_user_id: i64,
) -> Result<(), String> {
    let namespace_oid = namespace_oid(conn, schema)?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_type(oid, typname, typnamespace, typowner, typlen, \
             typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type) \
             VALUES ({type_oid}, '{}', {namespace_oid}, {owner_user_id}, -1, FALSE, 'c', 'C', TRUE, {rel_oid}, 0, 0, 'STRUCT')",
            sql_string(relation)
        ),
        [],
    )
    .map_err(|e| format!("write relation row type failed: {e}"))?;

    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_class(oid, relname, relnamespace, reltype, relowner, \
             relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message) \
             VALUES ({rel_oid}, '{}', {namespace_oid}, {type_oid}, {owner_user_id}, '{}', 'p', {}, 0, FALSE, FALSE, '', '', 'active', '')",
            sql_string(relation),
            sql_string(relkind),
            columns.len()
        ),
        [],
    )
    .map_err(|e| format!("write pg_class failed: {e}"))?;

    for column in columns {
        insert_attribute_row(conn, rel_oid, column)?;
    }

    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_relation_ext(relid, managed_kind, storage_mode, visibility, \
             partition_key, partition_key_type, partition_unit, retention_count, generated_sql, properties_json, created_at, updated_at) \
             VALUES ({rel_oid}, '{}', 'memory', '{}', '', '', '', 0, '{}', '{{}}', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            sql_string(managed_kind),
            sql_string(visibility),
            sql_string(generated_sql)
        ),
        [],
    )
    .map_err(|e| format!("write rs_relation_ext failed: {e}"))?;

    Ok(())
}

fn insert_attribute_row(
    conn: &Connection,
    rel_oid: i64,
    column: &CatalogColumn,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_attribute(attrelid, attname, atttypid, attnum, atttypmod, \
             attnotnull, atthasdef, attisdropped, attidentity, attgenerated, attoptions) \
             VALUES ({rel_oid}, '{}', {}, {}, -1, {}, {}, FALSE, '', '', '')",
            sql_string(&column.name),
            column.pg_type_oid,
            column.attnum,
            sql_bool(column.not_null),
            sql_bool(column.default_expr.is_some())
        ),
        [],
    )
    .map_err(|e| format!("write pg_attribute failed: {e}"))?;

    if let Some(default_expr) = &column.default_expr {
        let default_oid = allocate_oid(conn)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_attrdef(oid, adrelid, adnum, adbin) \
                 VALUES ({default_oid}, {rel_oid}, {}, '{}')",
                column.attnum,
                sql_string(default_expr)
            ),
            [],
        )
        .map_err(|e| format!("write pg_attrdef failed: {e}"))?;
    }
    Ok(())
}

fn update_partition_relation_ext(
    conn: &Connection,
    rel_oid: i64,
    partition_key: &str,
    partition_key_type: &str,
    partition_unit: &str,
    retention_count: i32,
    generated_sql: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation_ext \
             SET partition_key = '{}', partition_key_type = '{}', partition_unit = '{}', \
                 retention_count = {retention_count}, generated_sql = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE relid = {rel_oid}",
            sql_string(partition_key),
            sql_string(partition_key_type),
            sql_string(partition_unit),
            sql_string(generated_sql)
        ),
        [],
    )
    .map_err(|e| format!("update partition relation extension failed: {e}"))?;
    Ok(())
}

fn validate_partition_key(
    create_table: &CreateTable,
    partition_key: &str,
    partition_unit: &str,
) -> Result<(String, i64), String> {
    if !matches!(partition_unit, "hour" | "day" | "month" | "year") {
        return Err(format!(
            "partition_unit must be one of hour, day, month, year: {partition_unit}"
        ));
    }

    let column = create_table
        .columns
        .iter()
        .find(|column| column.name.value.eq_ignore_ascii_case(partition_key))
        .ok_or_else(|| format!("partition key column does not exist: {partition_key}"))?;
    if column
        .options
        .iter()
        .any(|option| matches!(option.option, ColumnOption::NotNull))
    {
        return Err("partition key column must allow NULL for null partition routing".into());
    }

    let type_text = column.data_type.to_string();
    let type_lower = type_text.to_ascii_lowercase();
    let key_type = if type_lower == "date" {
        if partition_unit == "hour" {
            return Err("DATE partition key does not support partition_unit = 'hour'".into());
        }
        "date"
    } else if type_lower.starts_with("timestamp") || type_lower == "datetime" {
        "timestamp"
    } else {
        return Err(format!(
            "partition key must be DATE or TIMESTAMP, got {type_text}"
        ));
    };
    Ok((
        key_type.to_string(),
        pg_type_oid_for_duckdb_type(&type_text)?,
    ))
}

fn physical_partition_name(parent: &str, partition_value: &str) -> String {
    let suffix = partition_value.trim_start_matches('_');
    format!("{parent}_{suffix}")
}

fn physical_partition_create_sql(partition_name: &str, create_table: &CreateTable) -> String {
    let columns = create_table
        .columns
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE {} ({columns})",
        quote_qualified("rsduck_internal", partition_name)
    )
}

fn partition_entrypoint_sql(schema: &str, table: &str, partitions: &[(&str, &str)]) -> String {
    let selects = partitions
        .iter()
        .map(|(partition_schema, partition_name)| {
            format!(
                "SELECT * FROM {}",
                quote_qualified(partition_schema, partition_name)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    format!(
        "CREATE OR REPLACE VIEW {} AS {selects}",
        quote_qualified(schema, table)
    )
}

fn ensure_active_partition(
    conn: &Connection,
    relation: &PartitionedRelation,
    partition_value: &str,
) -> Result<String, String> {
    if let Some(child) = active_partition_by_value(conn, relation.oid, partition_value)? {
        return Ok(child.relname);
    }
    if partition_value == "_null" {
        return Err("managed partitioned table is missing active null partition".into());
    }
    create_range_partition(conn, relation, partition_value)
}

fn active_partition_by_value(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
) -> Result<Option<ActivePartitionChild>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.child_relid, n.nspname, c.relname, c.status \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
               AND p.partition_value = '{}' \
               AND p.status = 'active'",
            sql_string(partition_value)
        ))
        .map_err(|e| format!("prepare partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(ActivePartitionChild {
        child_oid: row
            .get(0)
            .map_err(|e| format!("read partition child oid failed: {e}"))?,
        schema: row
            .get(1)
            .map_err(|e| format!("read partition schema failed: {e}"))?,
        relname: row
            .get(2)
            .map_err(|e| format!("read partition relation failed: {e}"))?,
        child_status: row
            .get(3)
            .map_err(|e| format!("read partition child status failed: {e}"))?,
    }))
}

fn create_range_partition(
    conn: &Connection,
    relation: &PartitionedRelation,
    partition_value: &str,
) -> Result<String, String> {
    let bounds = partition_bounds(partition_value, &relation.partition_unit)?;
    let child_relname = physical_partition_name(&relation.name, partition_value);
    if relation_exists(conn, "rsduck_internal", &child_relname)? {
        return Err(format!(
            "managed physical partition relation already exists: rsduck_internal.{child_relname}"
        ));
    }

    let child_oid = allocate_oid(conn)?;
    let child_type_oid = allocate_oid(conn)?;
    let create_sql = format!(
        "CREATE TABLE {} AS SELECT * FROM {} WHERE 1 = 0",
        quote_qualified("rsduck_internal", &child_relname),
        quote_qualified(&relation.schema, &relation.name)
    );
    conn.execute(&create_sql, [])
        .map_err(|e| format!("execute DuckDB CREATE partition failed: {e}"))?;
    let columns = load_duckdb_columns(conn, "rsduck_internal", &child_relname)?;
    insert_relation_rows(
        conn,
        child_oid,
        child_type_oid,
        "rsduck_internal",
        &child_relname,
        "r",
        "physical_partition",
        "internal",
        "",
        &columns,
        ADMIN_USER_ID,
    )?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.pg_class \
             SET relispartition = TRUE, relpartbound = '{}' \
             WHERE oid = {child_oid}",
            sql_string(&format!("[{}, {})", bounds.lower_bound, bounds.upper_bound))
        ),
        [],
    )
    .map_err(|e| format!("mark physical partition pg_class failed: {e}"))?;
    update_partition_relation_ext(
        conn,
        child_oid,
        &relation.partition_key,
        &relation.partition_key_type,
        &relation.partition_unit,
        0,
        "",
    )?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_partition(parent_relid, child_relid, partition_value, \
             partition_unit, lower_bound, upper_bound, is_null_partition, status, row_count, min_ts, \
             max_ts, checksum, created_at, activated_at, dropped_at, error_message) \
             VALUES ({}, {child_oid}, '{}', '{}', TIMESTAMP '{}', TIMESTAMP '{}', FALSE, 'active', \
             0, NULL, NULL, '', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, '')",
            relation.oid,
            sql_string(&bounds.value),
            sql_string(&relation.partition_unit),
            bounds.lower_bound,
            bounds.upper_bound
        ),
        [],
    )
    .map_err(|e| format!("write range partition metadata failed: {e}"))?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
             VALUES ({PG_CLASS_CLASSOID}, {}, 0, {PG_CLASS_CLASSOID}, {child_oid}, 0, 'n')",
            relation.oid
        ),
        [],
    )
    .map_err(|e| format!("write range partition dependency failed: {e}"))?;
    Ok(child_relname)
}

fn partition_bounds(
    partition_value: &str,
    partition_unit: &str,
) -> Result<PartitionBounds, String> {
    let lower_bound = match partition_unit {
        "hour" => NaiveDateTime::parse_from_str(partition_value, "%Y%m%d%H")
            .map_err(|_| format!("invalid hour partition_value: {partition_value}"))?,
        "day" => NaiveDate::parse_from_str(partition_value, "%Y%m%d")
            .map_err(|_| format!("invalid day partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid day partition_value: {partition_value}"))?,
        "month" => NaiveDate::parse_from_str(&format!("{partition_value}01"), "%Y%m%d")
            .map_err(|_| format!("invalid month partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid month partition_value: {partition_value}"))?,
        "year" => NaiveDate::parse_from_str(&format!("{partition_value}0101"), "%Y%m%d")
            .map_err(|_| format!("invalid year partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid year partition_value: {partition_value}"))?,
        _ => return Err(format!("unsupported partition_unit: {partition_unit}")),
    };
    let upper_bound = match partition_unit {
        "hour" => lower_bound + Duration::hours(1),
        "day" => lower_bound + Duration::days(1),
        "month" => add_months(lower_bound, 1)?,
        "year" => add_months(lower_bound, 12)?,
        _ => return Err(format!("unsupported partition_unit: {partition_unit}")),
    };
    Ok(PartitionBounds {
        value: partition_value.to_string(),
        lower_bound,
        upper_bound,
    })
}

fn add_months(value: NaiveDateTime, months: u32) -> Result<NaiveDateTime, String> {
    let total_month = value.year() * 12 + value.month0() as i32 + months as i32;
    let year = total_month.div_euclid(12);
    let month0 = total_month.rem_euclid(12) as u32;
    NaiveDate::from_ymd_opt(year, month0 + 1, value.day())
        .and_then(|date| date.and_hms_opt(value.hour(), value.minute(), value.second()))
        .ok_or_else(|| format!("invalid month arithmetic for {value}"))
}

fn refresh_partition_entrypoint(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let partitions = active_partition_children(conn, parent_oid)?;
    let active_physical = partitions
        .iter()
        .map(|partition| (partition.schema.as_str(), partition.relname.as_str()))
        .collect::<Vec<_>>();
    let sql = partition_entrypoint_sql(schema, relname, &active_physical);
    rebuild_partition_entrypoint(conn, parent_oid, &sql)?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.pg_depend \
             WHERE classid = {PG_CLASS_CLASSOID} AND objid = {parent_oid} \
               AND refclassid = {PG_CLASS_CLASSOID}"
        ),
        [],
    )
    .map_err(|e| format!("delete partition dependencies failed: {e}"))?;
    for partition in partitions {
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({PG_CLASS_CLASSOID}, {parent_oid}, 0, {PG_CLASS_CLASSOID}, {}, 0, 'n')",
                partition.child_oid
            ),
            [],
        )
        .map_err(|e| format!("write partition dependency failed: {e}"))?;
    }
    Ok(())
}

fn update_partition_stats(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
    inserted_rows: i64,
    route_ts: Option<NaiveDateTime>,
) -> Result<(), String> {
    let ts_update = route_ts
        .map(|dt| {
            format!(
                ", min_ts = CASE WHEN min_ts IS NULL OR TIMESTAMP '{dt}' < min_ts THEN TIMESTAMP '{dt}' ELSE min_ts END, \
                 max_ts = CASE WHEN max_ts IS NULL OR TIMESTAMP '{dt}' > max_ts THEN TIMESTAMP '{dt}' ELSE max_ts END"
            )
        })
        .unwrap_or_default();
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET row_count = row_count + {inserted_rows}{ts_update} \
             WHERE parent_relid = {parent_oid} AND partition_value = '{}'",
            sql_string(partition_value)
        ),
        [],
    )
    .map_err(|e| format!("update partition stats failed: {e}"))?;
    Ok(())
}

fn insert_create_table_constraints(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    columns: &[CatalogColumn],
    create_table: &CreateTable,
) -> Result<(), String> {
    let namespace_oid = namespace_oid(conn, schema)?;
    for (idx, column) in create_table.columns.iter().enumerate() {
        for option in &column.options {
            if matches!(option.option, ColumnOption::PrimaryKey(_)) {
                let con_oid = allocate_oid(conn)?;
                let conname = option
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_pkey"));
                let attnum = columns.get(idx).map(|c| c.attnum).ok_or_else(|| {
                    format!("primary key column is not materialized: {}", column.name)
                })?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "p",
                    rel_oid,
                    &attnum.to_string(),
                    "",
                )?;
            }
        }
    }

    for constraint in &create_table.constraints {
        match constraint {
            TableConstraint::PrimaryKey(pk) => {
                let con_oid = allocate_oid(conn)?;
                let conname = pk
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_pkey"));
                let conkey = index_columns_to_attnums(&pk.columns, columns)?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "p",
                    rel_oid,
                    &conkey,
                    "",
                )?;
            }
            TableConstraint::Unique(unique) => {
                let con_oid = allocate_oid(conn)?;
                let conname = unique
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_key"));
                let conkey = index_columns_to_attnums(&unique.columns, columns)?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "u",
                    rel_oid,
                    &conkey,
                    "",
                )?;
            }
            TableConstraint::Check(check) => {
                let con_oid = allocate_oid(conn)?;
                let conname = check
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_check"));
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "c",
                    rel_oid,
                    "",
                    &check.expr.to_string(),
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn insert_constraint(
    conn: &Connection,
    oid: i64,
    conname: &str,
    namespace_oid: i64,
    contype: &str,
    rel_oid: i64,
    conkey: &str,
    conbin: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_constraint(oid, conname, connamespace, contype, conrelid, \
             conindid, conkey, confrelid, confkey, convalidated, conbin) \
             VALUES ({oid}, '{}', {namespace_oid}, '{}', {rel_oid}, 0, '{}', 0, '', TRUE, '{}')",
            sql_string(conname),
            sql_string(contype),
            sql_string(conkey),
            sql_string(conbin)
        ),
        [],
    )
    .map_err(|e| format!("write pg_constraint failed: {e}"))?;
    Ok(())
}

fn index_columns_to_attnums(
    index_columns: &[sqlparser::ast::IndexColumn],
    columns: &[CatalogColumn],
) -> Result<String, String> {
    let mut attnums = Vec::with_capacity(index_columns.len());
    for index_column in index_columns {
        let column_name = index_column.column.expr.to_string();
        let attnum = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&column_name))
            .map(|column| column.attnum)
            .ok_or_else(|| format!("constraint references unknown column: {column_name}"))?;
        attnums.push(attnum.to_string());
    }
    Ok(attnums.join(","))
}

fn simple_index_column_names(
    index_columns: &[sqlparser::ast::IndexColumn],
) -> Result<Vec<String>, String> {
    let mut names = Vec::with_capacity(index_columns.len());
    for index_column in index_columns {
        if index_column.operator_class.is_some() || index_column.column.with_fill.is_some() {
            return Err("index column options are not supported by rsduck catalog".into());
        }
        match &index_column.column.expr {
            Expr::Identifier(ident) => names.push(ident.value.clone()),
            _ => return Err("expression index is not supported by rsduck catalog".into()),
        }
    }
    if names.is_empty() {
        return Err("CREATE INDEX requires at least one column".into());
    }
    Ok(names)
}

fn load_duckdb_columns(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<CatalogColumn>, String> {
    let sql = format!(
        "SELECT column_name, data_type, is_nullable, column_default, column_index \
         FROM duckdb_columns() \
         WHERE schema_name = '{}' AND table_name = '{}' AND internal = FALSE \
         ORDER BY column_index",
        sql_string(schema),
        sql_string(relation)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare duckdb_columns query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query duckdb_columns failed: {e}"))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read duckdb_columns failed: {e}"))?
    {
        let name: String = row
            .get(0)
            .map_err(|e| format!("read column_name failed: {e}"))?;
        let duckdb_type: String = row
            .get(1)
            .map_err(|e| format!("read data_type failed: {e}"))?;
        let is_nullable: bool = row
            .get(2)
            .map_err(|e| format!("read is_nullable failed: {e}"))?;
        let default_expr: Option<String> = row
            .get(3)
            .map_err(|e| format!("read column_default failed: {e}"))?;
        let column_index: i32 = row
            .get(4)
            .map_err(|e| format!("read column_index failed: {e}"))?;
        columns.push(CatalogColumn {
            name,
            pg_type_oid: pg_type_oid_for_duckdb_type(&duckdb_type)?,
            attnum: column_index,
            not_null: !is_nullable,
            default_expr,
        });
    }
    if columns.is_empty() {
        return Err(format!(
            "DuckDB relation has no columns: {schema}.{relation}"
        ));
    }
    Ok(columns)
}

fn pg_type_oid_for_duckdb_type(duckdb_type: &str) -> Result<i64, String> {
    let lower = duckdb_type.to_ascii_lowercase();
    if lower == "boolean" || lower == "bool" {
        Ok(16)
    } else if lower == "smallint" || lower == "int2" || lower == "utinyint" || lower == "tinyint" {
        Ok(21)
    } else if lower == "integer" || lower == "int" || lower == "int4" {
        Ok(23)
    } else if lower == "bigint" || lower == "int8" {
        Ok(20)
    } else if lower == "real" || lower == "float" || lower == "float4" {
        Ok(700)
    } else if lower == "double" || lower == "double precision" || lower == "float8" {
        Ok(701)
    } else if lower.starts_with("decimal") || lower.starts_with("numeric") {
        Ok(1700)
    } else if lower == "varchar" || lower.starts_with("varchar(") {
        Ok(1043)
    } else if lower == "text" || lower == "string" {
        Ok(25)
    } else if lower == "date" {
        Ok(1082)
    } else if lower == "time" || lower.starts_with("time(") {
        Ok(1083)
    } else if lower.starts_with("timestamp") || lower == "datetime" {
        Ok(1114)
    } else {
        Err(format!(
            "unsupported DuckDB type for rsduck catalog: {duckdb_type}"
        ))
    }
}

fn create_catalog_storage(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        CREATE SCHEMA IF NOT EXISTS rsduck_catalog;
        CREATE SCHEMA IF NOT EXISTS rsduck_internal;

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_catalog_version (
            id BIGINT PRIMARY KEY,
            schema_version BIGINT NOT NULL,
            catalog_epoch BIGINT NOT NULL,
            catalog_checksum VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_oid_alloc (
            id BIGINT PRIMARY KEY,
            next_oid BIGINT NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_catalog_journal (
            journal_id BIGINT PRIMARY KEY,
            catalog_epoch BIGINT NOT NULL,
            mutation_type VARCHAR NOT NULL,
            target_oid BIGINT NOT NULL,
            request_json VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            error_message VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            applied_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_namespace (
            oid BIGINT PRIMARY KEY,
            nspname VARCHAR NOT NULL UNIQUE,
            nspowner BIGINT NOT NULL,
            nspacl VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_type (
            oid BIGINT PRIMARY KEY,
            typname VARCHAR NOT NULL,
            typnamespace BIGINT NOT NULL,
            typowner BIGINT NOT NULL,
            typlen INT NOT NULL,
            typbyval BOOLEAN NOT NULL,
            typtype VARCHAR NOT NULL,
            typcategory VARCHAR NOT NULL,
            typisdefined BOOLEAN NOT NULL,
            typrelid BIGINT NOT NULL,
            typelem BIGINT NOT NULL,
            typarray BIGINT NOT NULL,
            rsduck_physical_type VARCHAR NOT NULL,
            UNIQUE(typnamespace, typname)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_class (
            oid BIGINT PRIMARY KEY,
            relname VARCHAR NOT NULL,
            relnamespace BIGINT NOT NULL,
            reltype BIGINT NOT NULL,
            relowner BIGINT NOT NULL,
            relkind VARCHAR NOT NULL,
            relpersistence VARCHAR NOT NULL,
            relnatts INT NOT NULL,
            reltuples DOUBLE NOT NULL,
            relhasindex BOOLEAN NOT NULL,
            relispartition BOOLEAN NOT NULL,
            relpartbound VARCHAR NOT NULL,
            reloptions VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            error_message VARCHAR NOT NULL,
            UNIQUE(relnamespace, relname)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_attribute (
            attrelid BIGINT NOT NULL,
            attname VARCHAR NOT NULL,
            atttypid BIGINT NOT NULL,
            attnum INT NOT NULL,
            atttypmod INT NOT NULL,
            attnotnull BOOLEAN NOT NULL,
            atthasdef BOOLEAN NOT NULL,
            attisdropped BOOLEAN NOT NULL,
            attidentity VARCHAR NOT NULL,
            attgenerated VARCHAR NOT NULL,
            attoptions VARCHAR NOT NULL,
            PRIMARY KEY(attrelid, attnum)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_attrdef (
            oid BIGINT PRIMARY KEY,
            adrelid BIGINT NOT NULL,
            adnum INT NOT NULL,
            adbin VARCHAR NOT NULL,
            UNIQUE(adrelid, adnum)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_constraint (
            oid BIGINT PRIMARY KEY,
            conname VARCHAR NOT NULL,
            connamespace BIGINT NOT NULL,
            contype VARCHAR NOT NULL,
            conrelid BIGINT NOT NULL,
            conindid BIGINT NOT NULL,
            conkey VARCHAR NOT NULL,
            confrelid BIGINT NOT NULL,
            confkey VARCHAR NOT NULL,
            convalidated BOOLEAN NOT NULL,
            conbin VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_index (
            indexrelid BIGINT PRIMARY KEY,
            indrelid BIGINT NOT NULL,
            indnatts INT NOT NULL,
            indnkeyatts INT NOT NULL,
            indisunique BOOLEAN NOT NULL,
            indisprimary BOOLEAN NOT NULL,
            indisvalid BOOLEAN NOT NULL,
            indkey VARCHAR NOT NULL,
            indexprs VARCHAR NOT NULL,
            indpred VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_depend (
            classid BIGINT NOT NULL,
            objid BIGINT NOT NULL,
            objsubid INT NOT NULL,
            refclassid BIGINT NOT NULL,
            refobjid BIGINT NOT NULL,
            refobjsubid INT NOT NULL,
            deptype VARCHAR NOT NULL,
            PRIMARY KEY(classid, objid, objsubid, refclassid, refobjid, refobjsubid)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.pg_description (
            objoid BIGINT NOT NULL,
            classoid BIGINT NOT NULL,
            objsubid INT NOT NULL,
            description VARCHAR NOT NULL,
            PRIMARY KEY(objoid, classoid, objsubid)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_relation_ext (
            relid BIGINT PRIMARY KEY,
            managed_kind VARCHAR NOT NULL,
            storage_mode VARCHAR NOT NULL,
            visibility VARCHAR NOT NULL,
            partition_key VARCHAR NOT NULL,
            partition_key_type VARCHAR NOT NULL,
            partition_unit VARCHAR NOT NULL,
            retention_count INT NOT NULL,
            generated_sql VARCHAR NOT NULL,
            properties_json VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_partition (
            parent_relid BIGINT NOT NULL,
            child_relid BIGINT NOT NULL,
            partition_value VARCHAR NOT NULL,
            partition_unit VARCHAR NOT NULL,
            lower_bound TIMESTAMP,
            upper_bound TIMESTAMP,
            is_null_partition BOOLEAN NOT NULL,
            status VARCHAR NOT NULL,
            row_count BIGINT NOT NULL,
            min_ts TIMESTAMP,
            max_ts TIMESTAMP,
            checksum VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            activated_at TIMESTAMP,
            dropped_at TIMESTAMP,
            error_message VARCHAR NOT NULL,
            PRIMARY KEY(parent_relid, child_relid)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_user (
            user_id BIGINT PRIMARY KEY,
            username VARCHAR NOT NULL UNIQUE,
            password_hash VARCHAR NOT NULL,
            password_algo VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            is_builtin BOOLEAN NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL,
            last_login_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_role (
            role_id BIGINT PRIMARY KEY,
            role_name VARCHAR NOT NULL UNIQUE,
            description VARCHAR NOT NULL,
            is_builtin BOOLEAN NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_user_role (
            user_id BIGINT NOT NULL,
            role_id BIGINT NOT NULL,
            granted_by BIGINT NOT NULL,
            created_at TIMESTAMP NOT NULL,
            PRIMARY KEY(user_id, role_id)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_privilege (
            privilege_id BIGINT PRIMARY KEY,
            principal_type VARCHAR NOT NULL,
            principal_id BIGINT NOT NULL,
            object_type VARCHAR NOT NULL,
            object_id BIGINT NOT NULL,
            action VARCHAR NOT NULL,
            granted_by BIGINT NOT NULL,
            created_at TIMESTAMP NOT NULL,
            UNIQUE(principal_type, principal_id, object_type, object_id, action)
        );
        ",
    )
    .map_err(|e| format!("create catalog storage failed: {e}"))?;
    Ok(())
}

fn insert_bootstrap_rows(conn: &Connection) -> Result<(), String> {
    let admin_password_hash = hash_password("admin")?;
    run_catalog_tx(conn, || {
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_oid_alloc(id, next_oid, updated_at) \
                 VALUES (1, {FIRST_USER_OID}, CURRENT_TIMESTAMP)"
            ),
            [],
        )
        .map_err(|e| format!("write oid allocator failed: {e}"))?;

        conn.execute(
            "INSERT INTO rsduck_catalog.rs_catalog_version(id, schema_version, catalog_epoch, catalog_checksum, status, created_at, updated_at) \
             VALUES (1, 1, 0, '', 'ready', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            [],
        )
        .map_err(|e| format!("write catalog version failed: {e}"))?;

        conn.execute_batch(&format!(
            "
            INSERT INTO rsduck_catalog.pg_namespace VALUES
              ({PG_CATALOG_NS}, 'pg_catalog', {ADMIN_USER_ID}, ''),
              ({INFORMATION_SCHEMA_NS}, 'information_schema', {ADMIN_USER_ID}, ''),
              ({RSDUCK_CATALOG_NS}, 'rsduck_catalog', {ADMIN_USER_ID}, ''),
              ({RSDUCK_INTERNAL_NS}, 'rsduck_internal', {ADMIN_USER_ID}, ''),
              ({MAIN_NS}, 'main', {ADMIN_USER_ID}, '');

            INSERT INTO rsduck_catalog.pg_type(oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type) VALUES
              (16, 'bool', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 1, TRUE, 'b', 'B', TRUE, 0, 0, 0, 'BOOLEAN'),
              (20, 'int8', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'BIGINT'),
              (21, 'int2', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 2, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'SMALLINT'),
              (23, 'int4', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'INTEGER'),
              (25, 'text', {PG_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'S', TRUE, 0, 0, 0, 'VARCHAR'),
              (700, 'float4', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'REAL'),
              (701, 'float8', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'DOUBLE'),
              (1043, 'varchar', {PG_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'S', TRUE, 0, 0, 0, 'VARCHAR'),
              (1082, 'date', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'DATE'),
              (1083, 'time', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'TIME'),
              (1114, 'timestamp', {PG_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'TIMESTAMP'),
              (1700, 'numeric', {PG_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'N', TRUE, 0, 0, 0, 'DECIMAL');

            INSERT INTO rsduck_catalog.rs_role VALUES
              ({ROLE_ADMIN_ID}, 'admin', 'full catalog and system administration', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_OPERATOR_ID}, 'operator', 'snapshot and catalog operations', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_DDL_ID}, 'ddl', 'schema and relation ddl operations', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_WRITER_ID}, 'writer', 'relation data writes', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_READER_ID}, 'reader', 'relation reads', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP);

            INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, status, is_builtin, created_at, updated_at, last_login_at)
              VALUES ({ADMIN_USER_ID}, 'admin', '{}', 'argon2id', 'active', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL);

            INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at)
              VALUES ({ADMIN_USER_ID}, {ROLE_ADMIN_ID}, {ADMIN_USER_ID}, CURRENT_TIMESTAMP);
            ",
            sql_string(&admin_password_hash)
        ))
        .map_err(|e| format!("write bootstrap catalog rows failed: {e}"))?;

        for action in ["manage_snapshot", "manage_catalog", "manage_user"] {
            let privilege_id = allocate_oid(conn)?;
            conn.execute(
                &format!(
                    "INSERT INTO rsduck_catalog.rs_privilege(privilege_id, principal_type, principal_id, object_type, object_id, action, granted_by, created_at) \
                     VALUES ({privilege_id}, 'role', {ROLE_ADMIN_ID}, 'system', 0, '{}', {ADMIN_USER_ID}, CURRENT_TIMESTAMP)",
                    sql_string(action)
                ),
                [],
            )
            .map_err(|e| format!("write admin privilege failed: {e}"))?;
        }

        Ok(0)
    })?;
    Ok(())
}

fn run_catalog_tx<F>(conn: &Connection, f: F) -> Result<usize, String>
where
    F: FnOnce() -> Result<usize, String>,
{
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("begin catalog mutation failed: {e}"))?;
    match f() {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("commit catalog mutation failed: {e}"))?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn insert_journal(
    conn: &Connection,
    mutation_type: &str,
    target_oid: i64,
    request: &str,
) -> Result<i64, String> {
    let journal_id = allocate_oid(conn)?;
    let next_epoch = catalog_epoch(conn)? + 1;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_catalog_journal(journal_id, catalog_epoch, mutation_type, target_oid, request_json, status, error_message, created_at, applied_at) \
             VALUES ({journal_id}, {next_epoch}, '{}', {target_oid}, '{}', 'pending', '', CURRENT_TIMESTAMP, NULL)",
            sql_string(mutation_type),
            sql_string(request)
        ),
        [],
    )
    .map_err(|e| format!("write catalog journal failed: {e}"))?;
    Ok(journal_id)
}

fn finish_journal(conn: &Connection, journal_id: i64) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_catalog_journal SET status = 'applied', applied_at = CURRENT_TIMESTAMP WHERE journal_id = {journal_id}"
        ),
        [],
    )
    .map_err(|e| format!("finish catalog journal failed: {e}"))?;
    conn.execute(
        "UPDATE rsduck_catalog.rs_catalog_version \
         SET catalog_epoch = catalog_epoch + 1, updated_at = CURRENT_TIMESTAMP \
         WHERE id = 1",
        [],
    )
    .map_err(|e| format!("increment catalog epoch failed: {e}"))?;
    Ok(())
}

fn allocate_oid(conn: &Connection) -> Result<i64, String> {
    let oid: i64 = conn
        .query_row(
            "SELECT next_oid FROM rsduck_catalog.rs_oid_alloc WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read next oid failed: {e}"))?;
    conn.execute(
        "UPDATE rsduck_catalog.rs_oid_alloc SET next_oid = next_oid + 1, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        [],
    )
    .map_err(|e| format!("advance oid allocator failed: {e}"))?;
    Ok(oid)
}

fn catalog_epoch(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT catalog_epoch FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read catalog epoch failed: {e}"))
}

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

fn schema_name_value(schema_name: &SchemaName) -> Result<String, String> {
    match schema_name {
        SchemaName::Simple(name) | SchemaName::NamedAuthorization(name, _) => {
            single_name_part(name)
        }
        SchemaName::UnnamedAuthorization(ident) => Ok(ident.value.clone()),
    }
}

fn relation_name(name: &ObjectName) -> Result<(String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation] => Ok(("main".to_string(), relation.clone())),
        [schema, relation] => Ok((schema.clone(), relation.clone())),
        _ => Err(format!("unsupported relation name: {name}")),
    }
}

fn relation_name_with_default(
    name: &ObjectName,
    default_schema: &str,
) -> Result<(String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation] => Ok((default_schema.to_string(), relation.clone())),
        [schema, relation] => Ok((schema.clone(), relation.clone())),
        _ => Err(format!("unsupported relation name: {name}")),
    }
}

fn comment_relation_name(
    object_type: CommentObject,
    object_name: &ObjectName,
) -> Result<(String, String), String> {
    if object_type == CommentObject::Column {
        let (schema, relation, _) = column_comment_target(object_name)?;
        Ok((schema, relation))
    } else {
        relation_name(object_name)
    }
}

fn column_comment_target(name: &ObjectName) -> Result<(String, String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation, column] => Ok(("main".to_string(), relation.clone(), column.clone())),
        [schema, relation, column] => Ok((schema.clone(), relation.clone(), column.clone())),
        _ => Err(format!("unsupported column name for COMMENT: {name}")),
    }
}

fn single_name_part(name: &ObjectName) -> Result<String, String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [part] => Ok(part.clone()),
        _ => Err(format!("unsupported schema name: {name}")),
    }
}

fn ident_parts(name: &ObjectName) -> Result<Vec<String>, String> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
            _ => Err(format!("unsupported object name part: {part}")),
        })
        .collect()
}

fn reject_reserved_schema(schema: &str) -> Result<(), String> {
    if is_reserved_schema(schema) {
        Err(format!(
            "reserved schema is managed by rsduck catalog: {schema}"
        ))
    } else {
        Ok(())
    }
}

fn is_reserved_schema(schema: &str) -> bool {
    matches!(
        schema.to_ascii_lowercase().as_str(),
        "pg_catalog" | "information_schema" | "rsduck_catalog" | "rsduck_internal"
    )
}

fn namespace_exists(conn: &Connection, schema: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_namespace WHERE lower(nspname) = lower('{}')",
                sql_string(schema)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check namespace failed: {e}"))?;
    Ok(count > 0)
}

fn ensure_user_schema_exists(conn: &Connection, schema: &str) -> Result<(), String> {
    if namespace_exists(conn, schema)? {
        Ok(())
    } else {
        Err(format!("schema does not exist: {schema}"))
    }
}

fn validate_username(username: &str) -> Result<(), String> {
    if username.is_empty() {
        return Err("username cannot be empty".into());
    }
    if !username
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "username contains unsupported characters: {username}"
        ));
    }
    Ok(())
}

fn user_exists(conn: &Connection, username: &str) -> Result<bool, String> {
    Ok(user_id_by_name_opt(conn, username)?.is_some())
}

fn user_id_by_name(conn: &Connection, username: &str) -> Result<i64, String> {
    user_id_by_name_opt(conn, username)?.ok_or_else(|| format!("user does not exist: {username}"))
}

fn user_id_by_name_opt(conn: &Connection, username: &str) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT user_id FROM rsduck_catalog.rs_user WHERE lower(username) = lower('{}')",
            sql_string(username)
        ))
        .map_err(|e| format!("prepare user lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query user lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read user lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read user id failed: {e}"))
}

fn role_id_by_name(conn: &Connection, role_name: &str) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT role_id FROM rsduck_catalog.rs_role WHERE lower(role_name) = lower('{}')",
            sql_string(role_name)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("role does not exist: {role_name}: {e}"))
}

fn namespace_oid(conn: &Connection, schema: &str) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT oid FROM rsduck_catalog.pg_namespace WHERE lower(nspname) = lower('{}')",
            sql_string(schema)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("namespace does not exist in catalog: {schema}: {e}"))
}

fn relation_exists(conn: &Connection, schema: &str, relation: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) \
                 FROM rsduck_catalog.pg_class c \
                 JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}')",
                sql_string(schema),
                sql_string(relation)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check relation failed: {e}"))?;
    Ok(count > 0)
}

fn find_relation_meta(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Option<RelationMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.oid, c.reltype, c.relkind, c.relispartition \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}') \
               AND c.status = 'active'",
            sql_string(schema),
            sql_string(relation)
        ))
        .map_err(|e| format!("prepare relation lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query relation lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read relation lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(RelationMeta {
        oid: row
            .get(0)
            .map_err(|e| format!("read relation oid failed: {e}"))?,
        reltype: row
            .get(1)
            .map_err(|e| format!("read relation type oid failed: {e}"))?,
        relkind: row
            .get(2)
            .map_err(|e| format!("read relation kind failed: {e}"))?,
        relispartition: row
            .get(3)
            .map_err(|e| format!("read relation partition flag failed: {e}"))?,
    }))
}

fn relation_oid(conn: &Connection, schema: &str, relation: &str) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT c.oid \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}') \
               AND c.status = 'active'",
            sql_string(schema),
            sql_string(relation)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("relation does not exist in catalog: {schema}.{relation}: {e}"))
}

fn column_exists(conn: &Connection, rel_oid: i64, column_name: &str) -> Result<bool, String> {
    Ok(column_attnum(conn, rel_oid, column_name)?.is_some())
}

fn column_attnum(
    conn: &Connection,
    rel_oid: i64,
    column_name: &str,
) -> Result<Option<i32>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT attnum FROM rsduck_catalog.pg_attribute \
             WHERE attrelid = {rel_oid} AND lower(attname) = lower('{}') AND attisdropped = FALSE",
            sql_string(column_name)
        ))
        .map_err(|e| format!("prepare column lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query column lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read column lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read column attnum failed: {e}"))
}

fn relation_kind(conn: &Connection, rel_oid: i64) -> Result<String, String> {
    conn.query_row(
        &format!("SELECT relkind FROM rsduck_catalog.pg_class WHERE oid = {rel_oid}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read relation kind failed: {e}"))
}

fn catalog_columns(conn: &Connection, rel_oid: i64) -> Result<Vec<CatalogColumn>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT a.attname, a.atttypid, a.attnum, a.attnotnull, d.adbin \
             FROM rsduck_catalog.pg_attribute a \
             LEFT JOIN rsduck_catalog.pg_attrdef d \
               ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = {rel_oid} AND a.attisdropped = FALSE \
             ORDER BY a.attnum"
        ))
        .map_err(|e| format!("prepare catalog column query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query catalog columns failed: {e}"))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read catalog columns failed: {e}"))?
    {
        columns.push(CatalogColumn {
            name: row
                .get(0)
                .map_err(|e| format!("read catalog attname failed: {e}"))?,
            pg_type_oid: row
                .get(1)
                .map_err(|e| format!("read catalog atttypid failed: {e}"))?,
            attnum: row
                .get(2)
                .map_err(|e| format!("read catalog attnum failed: {e}"))?,
            not_null: row
                .get(3)
                .map_err(|e| format!("read catalog attnotnull failed: {e}"))?,
            default_expr: row
                .get(4)
                .map_err(|e| format!("read catalog adbin failed: {e}"))?,
        });
    }
    Ok(columns)
}

fn ensure_drop_type(object_type: ObjectType, meta: &RelationMeta) -> Result<(), String> {
    let ok = match object_type {
        ObjectType::Table => meta.relkind == "r" || meta.relkind == "p",
        ObjectType::View => meta.relkind == "v",
        ObjectType::Index => meta.relkind == "i",
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "DROP {object_type} cannot drop relation with relkind={}",
            meta.relkind
        ))
    }
}

fn drop_relation_dependencies(
    conn: &Connection,
    meta: &RelationMeta,
    cascade: bool,
) -> Result<(), String> {
    let dependents = dependent_relation_oids(conn, meta.oid)?;
    if !dependents.is_empty() && !cascade {
        return Err("cannot drop relation with dependent objects without CASCADE".into());
    }
    for dependent_oid in dependents {
        if let Some(dependent) = relation_meta_by_oid(conn, dependent_oid)? {
            delete_relation_catalog(conn, &dependent)?;
        }
    }
    Ok(())
}

fn dependent_relation_oids(conn: &Connection, rel_oid: i64) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT objid FROM rsduck_catalog.pg_depend \
             WHERE refclassid = {PG_CLASS_CLASSOID} AND refobjid = {rel_oid} \
               AND classid = {PG_CLASS_CLASSOID}"
        ))
        .map_err(|e| format!("prepare dependent lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query dependent lookup failed: {e}"))?;
    let mut oids = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read dependent lookup failed: {e}"))?
    {
        oids.push(
            row.get(0)
                .map_err(|e| format!("read dependent oid failed: {e}"))?,
        );
    }
    Ok(oids)
}

fn relation_meta_by_oid(conn: &Connection, rel_oid: i64) -> Result<Option<RelationMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT oid, reltype, relkind, relispartition \
             FROM rsduck_catalog.pg_class WHERE oid = {rel_oid}"
        ))
        .map_err(|e| format!("prepare relation-by-oid lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query relation-by-oid lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read relation-by-oid lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(RelationMeta {
        oid: row
            .get(0)
            .map_err(|e| format!("read dependent relation oid failed: {e}"))?,
        reltype: row
            .get(1)
            .map_err(|e| format!("read dependent relation type failed: {e}"))?,
        relkind: row
            .get(2)
            .map_err(|e| format!("read dependent relation kind failed: {e}"))?,
        relispartition: row
            .get(3)
            .map_err(|e| format!("read dependent relation partition flag failed: {e}"))?,
    }))
}

fn execute_physical_drop(
    conn: &Connection,
    object_type: ObjectType,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let keyword = match object_type {
        ObjectType::Table => "TABLE",
        ObjectType::View => "VIEW",
        ObjectType::Index => "INDEX",
        _ => return Err(format!("DROP {object_type} is not supported")),
    };
    conn.execute(
        &format!("DROP {keyword} {}", quote_qualified(schema, relname)),
        [],
    )
    .map_err(|e| format!("execute DuckDB DROP {keyword} failed: {e}"))?;
    Ok(())
}

fn delete_relation_catalog(conn: &Connection, meta: &RelationMeta) -> Result<(), String> {
    let table_oid: Option<i64> = if meta.relkind == "i" {
        conn.query_row(
            &format!(
                "SELECT indrelid FROM rsduck_catalog.pg_index WHERE indexrelid = {}",
                meta.oid
            ),
            [],
            |row| row.get(0),
        )
        .ok()
    } else {
        None
    };

    for sql in [
        format!(
            "DELETE FROM rsduck_catalog.pg_depend WHERE objid = {} OR refobjid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_description WHERE objoid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_attrdef WHERE adrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_attribute WHERE attrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_constraint WHERE conrelid = {} OR conindid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_index WHERE indexrelid = {} OR indrelid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_relation_ext WHERE relid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_partition WHERE parent_relid = {} OR child_relid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_type WHERE oid = {} OR typrelid = {}",
            meta.reltype, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.pg_class WHERE oid = {}",
            meta.oid
        ),
    ] {
        conn.execute(&sql, [])
            .map_err(|e| format!("delete relation catalog rows failed: {e}"))?;
    }

    if let Some(table_oid) = table_oid {
        let index_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM rsduck_catalog.pg_index WHERE indrelid = {table_oid}"
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count remaining indexes failed: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class SET relhasindex = {} WHERE oid = {table_oid}",
                sql_bool(index_count > 0)
            ),
            [],
        )
        .map_err(|e| format!("update relhasindex after drop failed: {e}"))?;
    }
    Ok(())
}

fn catalog_exists(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = 'rsduck_catalog' AND table_name = 'rs_catalog_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check catalog existence failed: {e}"))?;
    Ok(count > 0)
}

fn catalog_version_row_exists(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check catalog version row failed: {e}"))?;
    Ok(count > 0)
}

fn has_user_objects(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables() \
             WHERE internal = FALSE \
               AND schema_name NOT IN ('information_schema', 'pg_catalog', 'rsduck_catalog', 'rsduck_internal')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check existing DuckDB user objects failed: {e}"))?;
    Ok(count > 0)
}

fn extract_read_relations(sql: &str) -> Vec<(String, String)> {
    let tokens = sql_tokens(sql);
    let mut relations = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if matches!(token.to_ascii_lowercase().as_str(), "from" | "join") {
            if let Some(next) = tokens.get(idx + 1) {
                if let Some(relation) = relation_from_token(next) {
                    relations.push(relation);
                }
            }
        }
    }
    relations
}

fn extract_relation_after(sql: &str, keyword: &str) -> Option<(String, String)> {
    let tokens = sql_tokens(sql);
    let keyword = keyword.to_ascii_lowercase();
    tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case(&keyword))
        .and_then(|idx| tokens.get(idx + 1))
        .and_then(|token| relation_from_token(token))
}

fn extract_first_relation_for_ddl(sql: &str) -> Option<(String, String)> {
    extract_relation_after(sql, "table")
        .or_else(|| extract_relation_after(sql, "view"))
        .or_else(|| extract_relation_after(sql, "index"))
        .or_else(|| extract_relation_after(sql, "on"))
}

fn sql_tokens(sql: &str) -> Vec<String> {
    sql.replace(',', " ")
        .replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(|token| token.trim_matches(';').trim_matches(',').trim().to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

fn relation_from_token(token: &str) -> Option<(String, String)> {
    let token = token
        .trim()
        .trim_matches(';')
        .trim_matches(',')
        .trim_matches('(')
        .trim_matches(')')
        .trim();
    if token.is_empty() || token.starts_with('$') {
        return None;
    }
    let lower = token.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "select"
            | "where"
            | "on"
            | "using"
            | "values"
            | "set"
            | "returning"
            | "if"
            | "not"
            | "exists"
    ) || lower.contains('(')
    {
        return None;
    }

    let parts = token
        .split('.')
        .map(|part| part.trim_matches('"').to_string())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [relation] if !relation.is_empty() => Some(("main".to_string(), relation.clone())),
        [schema, relation] if !schema.is_empty() && !relation.is_empty() => {
            Some((schema.clone(), relation.clone()))
        }
        _ => None,
    }
}

fn quoted_literals(sql: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\'' {
            continue;
        }
        let mut value = String::new();
        while let Some(next) = chars.next() {
            if next == '\'' {
                if matches!(chars.peek(), Some('\'')) {
                    let _ = chars.next();
                    value.push('\'');
                    continue;
                }
                break;
            }
            value.push(next);
        }
        literals.push(value);
    }
    literals
}

fn privilege_args(current_user: &str, args: &[String]) -> Result<(String, String, String), String> {
    match args {
        [object, privilege] => Ok((
            current_user.to_string(),
            object.clone(),
            privilege.to_ascii_lowercase(),
        )),
        [user, object, privilege, ..] => {
            Ok((user.clone(), object.clone(), privilege.to_ascii_lowercase()))
        }
        _ => Err("invalid privilege function arguments".into()),
    }
}

fn table_privilege_action(privilege: &str) -> &str {
    if privilege.contains("select") || privilege.contains("read") {
        "read"
    } else if privilege.contains("insert")
        || privilege.contains("update")
        || privilege.contains("delete")
        || privilege.contains("write")
    {
        "write"
    } else {
        "ddl"
    }
}

fn schema_privilege_action(privilege: &str) -> &str {
    if privilege.contains("usage") || privilege.contains("read") {
        "read"
    } else {
        "ddl"
    }
}

fn normalize_for_guard(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .to_ascii_lowercase()
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn statement_command(statement: &Statement) -> &'static str {
    match statement {
        Statement::CreateSchema { .. }
        | Statement::CreateTable(_)
        | Statement::CreateView(_)
        | Statement::CreateIndex(_)
        | Statement::CreateUser(_) => "CREATE",
        Statement::Drop { .. } => "DROP",
        Statement::AlterTable(_)
        | Statement::AlterSchema(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. }
        | Statement::AlterUser(_) => "ALTER",
        Statement::Comment { .. } => "COMMENT",
        Statement::Grant(_) => "GRANT",
        Statement::Revoke(_) => "REVOKE",
        _ => "SQL",
    }
}

fn sql_bool(value: bool) -> &'static str {
    if value {
        "TRUE"
    } else {
        "FALSE"
    }
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_qualified(schema: &str, relation: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(relation))
}

#[cfg(test)]
mod tests {
    use super::{
        allocate_oid, authorize_snapshot, authorize_sql, bootstrap_fresh,
        evaluate_privilege_function, execute_catalog_aware_write, execute_catalog_aware_write_as,
        hash_password, namespace_oid, relation_oid, sql_string, validate_after_start,
        verify_password,
    };
    use duckdb::Connection;

    #[test]
    fn bootstrap_creates_default_admin_and_roles() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        validate_after_start(&conn).unwrap();

        let (username, hash, algo): (String, String, String) = conn
            .query_row(
                "SELECT username, password_hash, password_algo FROM rsduck_catalog.rs_user WHERE username = 'admin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(username, "admin");
        assert_eq!(algo, "argon2id");
        assert!(verify_password("admin", &hash));

        let role_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM rsduck_catalog.rs_role", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(role_count, 5);
    }

    #[test]
    fn authenticate_default_admin_uses_catalog_password_hash() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();

        let user_id = super::authenticate_user(&conn, "admin", "admin").unwrap();
        assert_eq!(user_id, 10);

        let err = super::authenticate_user(&conn, "admin", "wrong").unwrap_err();
        assert!(err.contains("invalid username or password"));
    }

    #[test]
    fn relation_permissions_are_enforced_for_non_admin_users() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();
        insert_test_user(&conn, 101, "reader").unwrap();

        let denied = authorize_sql(&conn, "reader", "SELECT * FROM quotes").unwrap_err();
        assert!(denied.contains("permission denied"));

        let main_oid = namespace_oid(&conn, "main").unwrap();
        insert_test_privilege(&conn, 101, "schema", main_oid, "read").unwrap();
        authorize_sql(&conn, "reader", "SELECT * FROM quotes").unwrap();

        let denied =
            authorize_sql(&conn, "reader", "INSERT INTO quotes VALUES ('A', 1.0)").unwrap_err();
        assert!(denied.contains("permission denied"));

        let quotes_oid = relation_oid(&conn, "main", "quotes").unwrap();
        insert_test_privilege(&conn, 101, "relation", quotes_oid, "write").unwrap();
        authorize_sql(&conn, "reader", "INSERT INTO quotes VALUES ('A', 1.0)").unwrap();

        let (column, allowed) = evaluate_privilege_function(
            &conn,
            "reader",
            "SELECT has_table_privilege('quotes', 'SELECT')",
        )
        .unwrap();
        assert_eq!(column, "has_table_privilege");
        assert!(allowed);
    }

    #[test]
    fn ddl_permission_sets_created_relation_owner() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        insert_test_user(&conn, 102, "ddl_user").unwrap();
        let main_oid = namespace_oid(&conn, "main").unwrap();
        insert_test_privilege(&conn, 102, "schema", main_oid, "ddl").unwrap();

        authorize_sql(&conn, "ddl_user", "CREATE TABLE owned_table(id INTEGER)").unwrap();
        execute_catalog_aware_write_as(&conn, "ddl_user", "CREATE TABLE owned_table(id INTEGER)")
            .unwrap();

        let owner: i64 = conn
            .query_row(
                "SELECT relowner FROM rsduck_catalog.pg_class WHERE relname = 'owned_table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, 102);
    }

    #[test]
    fn snapshot_permission_uses_system_privileges_and_operator_role() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        insert_test_user(&conn, 103, "plain").unwrap();
        insert_test_user(&conn, 104, "snapshot_user").unwrap();
        insert_test_user(&conn, 105, "operator_user").unwrap();

        let denied = authorize_snapshot(&conn, "plain").unwrap_err();
        assert!(denied.contains("permission denied"));

        insert_test_privilege(&conn, 104, "system", 0, "manage_snapshot").unwrap();
        authorize_snapshot(&conn, "snapshot_user").unwrap();

        conn.execute(
            "INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at) \
             VALUES (105, 21, 10, CURRENT_TIMESTAMP)",
            [],
        )
        .unwrap();
        authorize_snapshot(&conn, "operator_user").unwrap();
    }

    #[test]
    fn create_table_writes_pg_class_and_attributes() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();

        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR NOT NULL, bar_time TIMESTAMP NOT NULL, close DOUBLE, PRIMARY KEY(code, bar_time))",
        )
        .unwrap();

        let relkind: String = conn
            .query_row(
                "SELECT relkind FROM rsduck_catalog.pg_class WHERE relname = 'kline_day'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(relkind, "r");

        let attr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_attribute a JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid WHERE c.relname = 'kline_day'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attr_count, 3);

        let pkey_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_constraint WHERE contype = 'p'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pkey_count, 1);
    }

    #[test]
    fn create_view_and_index_write_catalog_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE VIEW quote_view AS SELECT code, close FROM quotes",
        )
        .unwrap();
        execute_catalog_aware_write(&conn, "CREATE INDEX idx_quotes_code ON quotes(code)").unwrap();

        let view_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname = 'quote_view' AND relkind = 'v'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_count, 1);

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_index i JOIN rsduck_catalog.pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_quotes_code'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }

    #[test]
    fn alter_table_add_column_updates_catalog_and_duckdb() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();

        execute_catalog_aware_write(
            &conn,
            "ALTER TABLE quotes ADD COLUMN volume BIGINT DEFAULT 0",
        )
        .unwrap();

        let (attnum, has_default): (i32, bool) = conn
            .query_row(
                "SELECT a.attnum, a.atthasdef \
                 FROM rsduck_catalog.pg_attribute a \
                 JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
                 WHERE c.relname = 'quotes' AND a.attname = 'volume'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attnum, 3);
        assert!(has_default);

        conn.execute("INSERT INTO quotes(code, close) VALUES ('A', 1.0)", [])
            .unwrap();
        let volume: i64 = conn
            .query_row("SELECT volume FROM quotes WHERE code = 'A'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(volume, 0);
    }

    #[test]
    fn comment_on_table_and_column_writes_pg_description() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();

        execute_catalog_aware_write(&conn, "COMMENT ON TABLE quotes IS 'quotes table'").unwrap();
        execute_catalog_aware_write(&conn, "COMMENT ON COLUMN quotes.close IS 'close price'")
            .unwrap();

        let table_comment: String = conn
            .query_row(
                "SELECT d.description \
                 FROM rsduck_catalog.pg_description d \
                 JOIN rsduck_catalog.pg_class c ON c.oid = d.objoid \
                 WHERE c.relname = 'quotes' AND d.objsubid = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_comment, "quotes table");

        let column_comment: String = conn
            .query_row(
                "SELECT d.description \
                 FROM rsduck_catalog.pg_description d \
                 JOIN rsduck_catalog.pg_class c ON c.oid = d.objoid \
                 JOIN rsduck_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.objsubid \
                 WHERE c.relname = 'quotes' AND a.attname = 'close'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(column_comment, "close price");
    }

    #[test]
    fn drop_index_and_table_updates_catalog() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();
        execute_catalog_aware_write(&conn, "CREATE INDEX idx_quotes_code ON quotes(code)").unwrap();

        execute_catalog_aware_write(&conn, "DROP INDEX idx_quotes_code").unwrap();
        let remaining_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname = 'idx_quotes_code'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_index_count, 0);
        let relhasindex: bool = conn
            .query_row(
                "SELECT relhasindex FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!relhasindex);

        execute_catalog_aware_write(&conn, "DROP TABLE quotes").unwrap();
        let remaining_table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_table_count, 0);

        let physical_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_tables() WHERE schema_name = 'main' AND table_name = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(physical_count, 0);
    }

    #[test]
    fn drop_table_with_dependent_index_requires_cascade() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();
        execute_catalog_aware_write(&conn, "CREATE INDEX idx_quotes_code ON quotes(code)").unwrap();

        let err = execute_catalog_aware_write(&conn, "DROP TABLE quotes").unwrap_err();
        assert!(err.contains("dependent objects"));

        execute_catalog_aware_write(&conn, "DROP TABLE quotes CASCADE").unwrap();
        let remaining_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname IN ('quotes', 'idx_quotes_code')",
                [],
                |row| row.get(0),
        )
        .unwrap();
        assert_eq!(remaining_count, 0);
    }

    #[test]
    fn create_managed_partitioned_table_creates_null_partition_and_entrypoint() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();

        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log (
                id BIGINT,
                user_id VARCHAR(64),
                access_time TIMESTAMP,
                content TEXT
             )
             PARTITION BY RANGE (access_time)
             WITH (
                partition_unit = 'day',
                retention = '30'
             )",
        )
        .unwrap();

        let (relkind, managed_kind, partition_key, partition_unit, retention): (
            String,
            String,
            String,
            String,
            i32,
        ) = conn
            .query_row(
                "SELECT c.relkind, ext.managed_kind, ext.partition_key, ext.partition_unit, ext.retention_count \
                 FROM rsduck_catalog.pg_class c \
                 JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid \
                 WHERE c.relname = 'ods_access_log'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(relkind, "p");
        assert_eq!(managed_kind, "range_partitioned_table");
        assert_eq!(partition_key, "access_time");
        assert_eq!(partition_unit, "day");
        assert_eq!(retention, 30);

        let null_partition_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) \
                 FROM rsduck_catalog.rs_partition p \
                 JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
                 WHERE c.relname = 'ods_access_log_null' \
                   AND p.partition_value = '_null' \
                   AND p.partition_unit = 'null' \
                   AND p.is_null_partition = TRUE \
                   AND p.status = 'active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_partition_count, 1);

        conn.execute(
            "INSERT INTO rsduck_internal.ods_access_log_null(id, user_id, access_time, content) \
             VALUES (1, 'u1', NULL, 'dirty')",
            [],
        )
        .unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM ods_access_log WHERE access_time IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, "dirty");
    }

    #[test]
    fn insert_into_partitioned_table_creates_partitions_and_routes_dirty_rows() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log (
                id BIGINT,
                user_id VARCHAR(64),
                access_time TIMESTAMP,
                content TEXT
             )
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();

        let affected = execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, user_id, access_time, content) VALUES
             (1, 'u1', TIMESTAMP '2026-07-01 10:00:00', 'ok-1'),
             (2, 'u2', '2026-07-02 08:30:00', 'ok-2'),
             (3, 'u3', NULL, 'null-key'),
             (4, 'u4', 'bad-time', 'dirty-key')",
        )
        .unwrap();
        assert_eq!(affected, Some(4));

        let partition_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_partition \
                 WHERE parent_relid = (
                    SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'ods_access_log'
                 ) AND status = 'active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(partition_count, 3);

        let ordinary_partition_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class \
                 WHERE relname IN ('ods_access_log_20260701', 'ods_access_log_20260702') \
                   AND relispartition = TRUE",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ordinary_partition_count, 2);

        let visible_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible_rows, 4);

        let dirty_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ods_access_log WHERE access_time IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dirty_rows, 2);

        let july_1_count: i64 = conn
            .query_row(
                "SELECT row_count FROM rsduck_catalog.rs_partition WHERE partition_value = '20260701'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(july_1_count, 1);

        let null_count: i64 = conn
            .query_row(
                "SELECT row_count FROM rsduck_catalog.rs_partition WHERE partition_value = '_null'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_count, 2);
    }

    #[test]
    fn alter_partitioned_table_add_column_updates_parent_and_partitions() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log (
                id BIGINT,
                access_time TIMESTAMP,
                content TEXT
             )
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time, content) VALUES
             (1, TIMESTAMP '2026-07-01 10:00:00', 'ok'),
             (2, NULL, 'dirty')",
        )
        .unwrap();

        execute_catalog_aware_write(
            &conn,
            "ALTER TABLE ods_access_log ADD COLUMN source TEXT DEFAULT 'web'",
        )
        .unwrap();

        let parent_attr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) \
                 FROM rsduck_catalog.pg_attribute a \
                 JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
                 WHERE c.relname = 'ods_access_log' AND a.attname = 'source'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent_attr_count, 1);

        let child_attr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) \
                 FROM rsduck_catalog.pg_attribute a \
                 JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
                 WHERE c.relname IN ('ods_access_log_20260701', 'ods_access_log_null') \
                   AND a.attname = 'source'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child_attr_count, 2);

        let source: String = conn
            .query_row(
                "SELECT source FROM ods_access_log WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "web");

        execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time, content, source) VALUES
             (3, TIMESTAMP '2026-07-02 09:00:00', 'ok-2', 'api')",
        )
        .unwrap();
        let inserted_source: String = conn
            .query_row(
                "SELECT source FROM ods_access_log WHERE id = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(inserted_source, "api");
    }

    #[test]
    fn drop_partitioned_table_removes_entrypoint_partitions_and_catalog() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time) VALUES
             (1, TIMESTAMP '2026-07-01 10:00:00'),
             (2, NULL)",
        )
        .unwrap();

        execute_catalog_aware_write(&conn, "DROP TABLE ods_access_log").unwrap();

        let class_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_class \
                 WHERE relname IN ('ods_access_log', 'ods_access_log_20260701', 'ods_access_log_null')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(class_count, 0);
        let partition_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_partition",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(partition_count, 0);
        let view_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_views() WHERE schema_name = 'main' AND view_name = 'ods_access_log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_count, 0);
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_tables() \
                 WHERE schema_name = 'rsduck_internal' \
                   AND table_name IN ('ods_access_log_20260701', 'ods_access_log_null')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }

    #[test]
    fn partitioned_table_validation_rejects_invalid_key_rules() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();

        let err = execute_catalog_aware_write(
            &conn,
            "CREATE TABLE bad_hour(id BIGINT, trade_date DATE)
             PARTITION BY RANGE (trade_date)
             WITH (partition_unit = 'hour', retention = '7')",
        )
        .unwrap_err();
        assert!(err.contains("DATE partition key does not support"));

        let err = execute_catalog_aware_write(
            &conn,
            "CREATE TABLE bad_not_null(id BIGINT, access_time TIMESTAMP NOT NULL)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '7')",
        )
        .unwrap_err();
        assert!(err.contains("must allow NULL"));
    }

    #[test]
    fn startup_validation_rebuilds_partition_entrypoint_from_catalog() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        conn.execute("DROP VIEW ods_access_log", []).unwrap();

        validate_after_start(&conn).unwrap();

        let view_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_views() WHERE schema_name = 'main' AND view_name = 'ods_access_log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_count, 1);
        let status: String = conn
            .query_row(
                "SELECT status FROM rsduck_catalog.pg_class WHERE relname = 'ods_access_log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn startup_validation_marks_partition_parent_unavailable_when_child_missing() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        conn.execute("DROP VIEW ods_access_log", []).unwrap();
        conn.execute("DROP TABLE rsduck_internal.ods_access_log_null", [])
            .unwrap();

        validate_after_start(&conn).unwrap();

        let parent_status: String = conn
            .query_row(
                "SELECT status FROM rsduck_catalog.pg_class WHERE relname = 'ods_access_log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent_status, "unavailable");

        let partition_status: String = conn
            .query_row(
                "SELECT status FROM rsduck_catalog.rs_partition WHERE partition_value = '_null'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(partition_status, "failed");
    }

    #[test]
    fn startup_validation_marks_missing_physical_table_unavailable() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();
        execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)")
            .unwrap();
        conn.execute("DROP TABLE quotes", []).unwrap();

        validate_after_start(&conn).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "unavailable");
    }

    #[test]
    fn reserved_schema_write_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        bootstrap_fresh(&conn).unwrap();

        let err =
            execute_catalog_aware_write(&conn, "CREATE TABLE rsduck_catalog.bad_table(id INTEGER)")
                .unwrap_err();
        assert!(err.contains("reserved schema"));
    }

    fn insert_test_user(conn: &Connection, user_id: i64, username: &str) -> Result<(), String> {
        let password_hash = hash_password("pw")?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, status, is_builtin, created_at, updated_at, last_login_at) \
                 VALUES ({user_id}, '{}', '{}', 'argon2id', 'active', FALSE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)",
                sql_string(username),
                sql_string(&password_hash)
            ),
            [],
        )
        .map_err(|e| format!("insert test user failed: {e}"))?;
        Ok(())
    }

    fn insert_test_privilege(
        conn: &Connection,
        user_id: i64,
        object_type: &str,
        object_id: i64,
        action: &str,
    ) -> Result<(), String> {
        let privilege_id = allocate_oid(conn)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_privilege(privilege_id, principal_type, principal_id, object_type, object_id, action, granted_by, created_at) \
                 VALUES ({privilege_id}, 'user', {user_id}, '{}', {object_id}, '{}', 10, CURRENT_TIMESTAMP)",
                sql_string(object_type),
                sql_string(action)
            ),
            [],
        )
        .map_err(|e| format!("insert test privilege failed: {e}"))?;
        Ok(())
    }
}

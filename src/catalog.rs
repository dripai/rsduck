use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use duckdb::Connection;
use rand_core::OsRng;
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnOption, CommentObject, CreateIndex, CreateTable,
    CreateView, Expr, ObjectName, ObjectNamePart, ObjectType, SchemaName, Statement,
    TableConstraint,
};
use sqlparser::dialect::DuckDbDialect;
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

pub fn authorize_sql(conn: &Connection, username: &str, sql: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    if principal.is_admin() {
        return Ok(());
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
        if relkind != "r" {
            return Err(format!(
                "ALTER TABLE ADD COLUMN only supports ordinary tables, got relkind={relkind}"
            ));
        }
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
            execute_physical_drop(conn, object_type, &schema, &relname)?;
            delete_relation_catalog(conn, &meta)?;
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
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
            attnum: column_index + 1,
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

fn parse_one_statement(sql: &str) -> Result<(Statement, String), String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("catalog sql parse failed: {e}"))?;
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
        | Statement::CreateIndex(_) => "CREATE",
        Statement::Drop { .. } => "DROP",
        Statement::AlterTable(_)
        | Statement::AlterSchema(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. } => "ALTER",
        Statement::Comment { .. } => "COMMENT",
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

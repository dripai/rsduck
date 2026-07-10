use super::*;

pub fn guard_external_sql(sql: &str) -> Result<(), String> {
    let normalized = normalize_for_guard(sql);
    for schema in [
        "rsduck_catalog",
        "rsduck_internal",
        "pg_catalog",
        "information_schema",
    ] {
        if normalized.contains(&format!("{schema}."))
            && (!matches!(schema, "pg_catalog" | "information_schema")
                || !is_catalog_projection_read(&normalized))
        {
            return Err(format!("reserved schema is managed by rsduck: {schema}"));
        }
    }
    Ok(())
}

pub fn guard_external_sql_as(username: &str, sql: &str) -> Result<(), String> {
    match guard_external_sql(sql) {
        Ok(()) => Ok(()),
        Err(err) => {
            warn!(
                target: "rsduck_audit",
                event = "reserved_schema_rejected",
                username = username,
                error = err.as_str()
            );
            Err(err)
        }
    }
}

pub(super) fn is_catalog_projection_read(normalized_sql: &str) -> bool {
    normalized_sql.starts_with("select ") || normalized_sql.starts_with("with ")
}

pub fn is_reserved_diagnostic_read(sql: &str) -> bool {
    let normalized = normalize_for_guard(sql);
    is_catalog_projection_read(&normalized)
        && (normalized.contains("rsduck_catalog.") || normalized.contains("rsduck_internal."))
}

pub fn authorize_reserved_diagnostic(
    conn: &Connection,
    username: &str,
    sql: &str,
) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    require_system_action(conn, &principal, "manage_catalog")?;
    info!(
        target: "rsduck_audit",
        event = "reserved_schema_diagnostic_read",
        username = username,
        sql = sql
    );
    Ok(())
}

pub fn looks_like_managed_partition_create(sql: &str) -> bool {
    normalize_for_guard(sql).starts_with("create table ")
        && find_keyword_phrase(sql, "partition by range").is_some()
}

pub fn looks_like_catalog_management_call(sql: &str) -> bool {
    parse_catalog_management_call(sql).is_some()
}

#[derive(Debug)]
pub(super) struct CatalogManagementCall {
    pub(super) name: String,
    pub(super) args: Vec<String>,
}

pub(super) fn parse_catalog_management_call(sql: &str) -> Option<CatalogManagementCall> {
    let normalized = normalize_for_guard(sql);
    let body = normalized.strip_prefix("call ")?;
    let open = body.find('(')?;
    let name = body[..open].trim().to_string();
    if !matches!(
        name.as_str(),
        "rsduck_run_partition_maintenance"
            | "rsduck_mark_partition_unavailable"
            | "rsduck_repair_partition"
    ) {
        return None;
    }
    Some(CatalogManagementCall {
        name,
        args: quoted_literals(sql),
    })
}

pub(super) fn execute_catalog_management_call(
    conn: &Connection,
    principal: &SessionPrincipal,
    call: CatalogManagementCall,
    sql: &str,
) -> Result<usize, String> {
    require_system_action(conn, principal, "manage_catalog")?;
    match call.name.as_str() {
        "rsduck_run_partition_maintenance" => {
            if !call.args.is_empty() {
                return Err("rsduck_run_partition_maintenance takes no arguments".into());
            }
            run_partition_maintenance(conn, sql)
        }
        "rsduck_mark_partition_unavailable" => {
            if call.args.len() != 3 {
                return Err(
                    "rsduck_mark_partition_unavailable requires relation, partition_value, reason"
                        .into(),
                );
            }
            let (schema, table) = relation_from_token(&call.args[0])
                .ok_or_else(|| format!("invalid relation name: {}", call.args[0]))?;
            mark_partition_unavailable(conn, &schema, &table, &call.args[1], &call.args[2], sql)
        }
        "rsduck_repair_partition" => {
            if call.args.len() != 2 {
                return Err("rsduck_repair_partition requires relation and partition_value".into());
            }
            let (schema, table) = relation_from_token(&call.args[0])
                .ok_or_else(|| format!("invalid relation name: {}", call.args[0]))?;
            repair_partition(conn, &schema, &table, &call.args[1], sql)
        }
        _ => Err(format!(
            "unsupported catalog management call: {}",
            call.name
        )),
    }
}

pub fn authorize_sql(conn: &Connection, username: &str, sql: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;

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
        | Statement::CreateRole(_)
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
        Statement::Drop { object_type, .. } => {
            if matches!(object_type, ObjectType::User | ObjectType::Role) {
                require_system_action(conn, &principal, "manage_user")
            } else if let Some(relation) = extract_first_relation_for_ddl(&normalized_sql) {
                require_relation_action(conn, &principal, &relation, "ddl")
            } else {
                require_system_action(conn, &principal, "manage_catalog")
            }
        }
        Statement::AlterTable(_)
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
        Statement::Call(_) if looks_like_catalog_management_call(&normalized_sql) => {
            require_system_action(conn, &principal, "manage_catalog")
        }
        _ if principal.is_admin() => Ok(()),
        _ => {
            let command = statement_command(&statement);
            audit_permission_denied(&principal.username, "statement", &command, "execute");
            Err(format!("permission denied for statement type: {command}"))
        }
    }
}

pub fn authorize_snapshot(conn: &Connection, username: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    require_system_action(conn, &principal, "manage_snapshot")
}

pub fn authorize_catalog_projection(conn: &Connection, username: &str) -> Result<(), String> {
    principal_for_username(conn, username).map(|_| ())
}

pub fn reject_unhandled_catalog_projection(sql: &str) -> Result<(), String> {
    let normalized = normalize_for_guard(sql);
    if let Some(name) = unsupported_catalog_relation(&normalized, "pg_catalog") {
        return Err(format!("unsupported pg_catalog relation: {name}"));
    }
    if let Some(name) = unsupported_catalog_relation(&normalized, "information_schema") {
        return Err(format!("unsupported information_schema relation: {name}"));
    }
    if normalized.contains("pg_catalog.") {
        return Err("unsupported pg_catalog query".into());
    }
    if normalized.contains("information_schema.") {
        return Err("unsupported information_schema query".into());
    }
    Ok(())
}

pub(super) fn extract_read_relations(sql: &str) -> Vec<(String, String)> {
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

pub(super) fn unsupported_catalog_relation(sql: &str, catalog_schema: &str) -> Option<String> {
    for (schema, relation) in extract_read_relations(sql) {
        if schema.eq_ignore_ascii_case(catalog_schema) {
            return Some(relation);
        }
    }
    None
}

pub(super) fn extract_relation_after(sql: &str, keyword: &str) -> Option<(String, String)> {
    let tokens = sql_tokens(sql);
    let keyword = keyword.to_ascii_lowercase();
    tokens
        .iter()
        .position(|token| token.eq_ignore_ascii_case(&keyword))
        .and_then(|idx| tokens.get(idx + 1))
        .and_then(|token| relation_from_token(token))
}

pub(super) fn extract_first_relation_for_ddl(sql: &str) -> Option<(String, String)> {
    extract_relation_after(sql, "table")
        .or_else(|| extract_relation_after(sql, "view"))
        .or_else(|| extract_relation_after(sql, "index"))
        .or_else(|| extract_relation_after(sql, "on"))
}

pub(super) fn sql_tokens(sql: &str) -> Vec<String> {
    sql.replace(',', " ")
        .replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(|token| token.trim_matches(';').trim_matches(',').trim().to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

pub(super) fn relation_from_token(token: &str) -> Option<(String, String)> {
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
    if matches!(lower.as_str(), "duckdb_functions" | "unnest") {
        return None;
    }
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
        .map(normalize_relation_identifier)
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [relation] if !relation.is_empty() => Some(("main".to_string(), relation.clone())),
        [schema, relation] if !schema.is_empty() && !relation.is_empty() => {
            Some((schema.clone(), relation.clone()))
        }
        _ => None,
    }
}

pub fn authorize_user_metadata(conn: &Connection, username: &str) -> Result<(), String> {
    let principal = principal_for_username(conn, username)?;
    require_system_action(conn, &principal, "manage_user")
}

fn normalize_relation_identifier(part: &str) -> String {
    let part = part.trim().trim_matches(|ch| matches!(ch, '"' | '`'));
    part.replace("``", "`").replace("\"\"", "\"")
}

pub(super) fn quoted_literals(sql: &str) -> Vec<String> {
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

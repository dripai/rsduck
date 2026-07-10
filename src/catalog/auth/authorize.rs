use super::*;

impl SessionPrincipal {
    pub(in crate::catalog) fn is_admin(&self) -> bool {
        self.roles.iter().any(|role| role == "admin")
    }
}

pub(in crate::catalog) fn require_system_action(
    conn: &Connection,
    principal: &SessionPrincipal,
    action: &str,
) -> Result<(), String> {
    if principal.is_admin() || has_explicit_privilege(conn, principal, "system", 0, action)? {
        return Ok(());
    }
    audit_permission_denied(&principal.username, "system", "0", action);
    Err(format!(
        "permission denied for user {}: system {action}",
        principal.username
    ))
}

pub(in crate::catalog) fn require_schema_action(
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
    audit_permission_denied(&principal.username, "schema", schema, action);
    Err(format!(
        "permission denied for user {}: schema {schema} {action}",
        principal.username
    ))
}

pub(in crate::catalog) fn require_relation_action(
    conn: &Connection,
    principal: &SessionPrincipal,
    relation: &(String, String),
    action: &str,
) -> Result<(), String> {
    let (schema, relname) = relation;
    let rel_oid = available_relation_oid(conn, schema, relname)?;
    let namespace_oid = namespace_oid(conn, schema)?;
    if principal.is_admin()
        || has_explicit_privilege(conn, principal, "relation", rel_oid, action)?
        || (action == "read"
            && has_explicit_privilege(conn, principal, "schema", namespace_oid, "read")?)
    {
        return Ok(());
    }
    audit_permission_denied(
        &principal.username,
        "relation",
        &format!("{schema}.{relname}"),
        action,
    );
    Err(format!(
        "permission denied for user {}: relation {}.{} {action}",
        principal.username, schema, relname
    ))
}

pub(in crate::catalog) fn audit_permission_denied(
    username: &str,
    scope: &str,
    object: &str,
    action: &str,
) {
    warn!(
        target: "rsduck_audit",
        event = "permission_denied",
        username = username,
        scope = scope,
        object = object,
        action = action
    );
}

pub(in crate::catalog) fn has_explicit_privilege(
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

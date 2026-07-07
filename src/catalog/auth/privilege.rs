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
        let (target_user, database, privilege) = privilege_args(current_user, &args)?;
        return Ok((
            "has_database_privilege".to_string(),
            has_database_privilege(conn, &target_user, &database, &privilege)?,
        ));
    }
    Err("unsupported privilege function".into())
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

fn database_privilege_action(privilege: &str) -> &str {
    if privilege.contains("create") {
        "manage_catalog"
    } else if privilege.contains("temporary") || privilege.contains("temp") {
        "manage_catalog"
    } else if privilege.contains("connect") {
        "manage_snapshot"
    } else {
        "manage_catalog"
    }
}


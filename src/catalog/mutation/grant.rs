fn grant_privileges(
    conn: &Connection,
    grant: &Grant,
    sql: &str,
    grantor_id: i64,
) -> Result<usize, String> {
    if grant.objects.is_none() && grant_contains_role_actions(&grant.privileges) {
        return grant_roles(conn, &grant.privileges, &grant.grantees, sql, grantor_id);
    }
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
    if revoke.objects.is_none() && grant_contains_role_actions(&revoke.privileges) {
        return revoke_roles(conn, &revoke.privileges, &revoke.grantees, sql);
    }
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

fn grant_roles(
    conn: &Connection,
    privileges: &Privileges,
    grantees: &[sqlparser::ast::Grantee],
    sql: &str,
    grantor_id: i64,
) -> Result<usize, String> {
    let role_ids = granted_role_ids(conn, privileges)?;
    let user_ids = role_grantee_user_ids(conn, grantees)?;
    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "grant_role", 0, sql)?;
        let mut affected = 0;
        for user_id in &user_ids {
            for role_id in &role_ids {
                affected += upsert_user_role(conn, *user_id, *role_id, grantor_id)?;
            }
        }
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

fn revoke_roles(
    conn: &Connection,
    privileges: &Privileges,
    grantees: &[sqlparser::ast::Grantee],
    sql: &str,
) -> Result<usize, String> {
    let role_ids = granted_role_ids(conn, privileges)?;
    let user_ids = role_grantee_user_ids(conn, grantees)?;
    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "revoke_role", 0, sql)?;
        let mut affected = 0;
        for user_id in &user_ids {
            for role_id in &role_ids {
                affected += conn
                    .execute(
                        &format!(
                            "DELETE FROM rsduck_catalog.rs_user_role \
                             WHERE user_id = {user_id} AND role_id = {role_id}"
                        ),
                        [],
                    )
                    .map_err(|e| format!("revoke role failed: {e}"))?;
            }
        }
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

fn grant_contains_role_actions(privileges: &Privileges) -> bool {
    matches!(
        privileges,
        Privileges::Actions(actions)
            if actions.iter().any(|action| matches!(action, Action::Role { .. }))
    )
}

fn granted_role_ids(conn: &Connection, privileges: &Privileges) -> Result<Vec<i64>, String> {
    let Privileges::Actions(actions) = privileges else {
        return Err("role grant requires explicit ROLE action".into());
    };
    let mut role_ids = Vec::new();
    for action in actions {
        let Action::Role { role } = action else {
            return Err("GRANT/REVOKE ROLE cannot be mixed with object privileges".into());
        };
        let role_name = single_name_part(role)?;
        let role_id = role_id_by_name(conn, &role_name)?;
        if !role_ids.contains(&role_id) {
            role_ids.push(role_id);
        }
    }
    if role_ids.is_empty() {
        return Err("GRANT/REVOKE ROLE requires at least one role".into());
    }
    Ok(role_ids)
}

fn role_grantee_user_ids(
    conn: &Connection,
    grantees: &[sqlparser::ast::Grantee],
) -> Result<Vec<i64>, String> {
    if grantees.is_empty() {
        return Err("GRANT/REVOKE ROLE requires at least one user".into());
    }
    let mut user_ids = Vec::new();
    for grantee in grantees {
        if grantee.grantee_type == GranteesType::Role {
            return Err("role inheritance is not supported by rsduck catalog".into());
        }
        let username = match &grantee.name {
            Some(GranteeName::ObjectName(name)) => single_name_part(name)?,
            Some(GranteeName::UserHost { user, .. }) => user.value.clone(),
            None => return Err("grantee name is required".into()),
        };
        let user_id = user_id_by_name(conn, &username)?;
        if !user_ids.contains(&user_id) {
            user_ids.push(user_id);
        }
    }
    Ok(user_ids)
}

fn upsert_user_role(
    conn: &Connection,
    user_id: i64,
    role_id: i64,
    grantor_id: i64,
) -> Result<usize, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_user_role \
                 WHERE user_id = {user_id} AND role_id = {role_id}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check existing role grant failed: {e}"))?;
    if count > 0 {
        return Ok(0);
    }
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at) \
             VALUES ({user_id}, {role_id}, {grantor_id}, CURRENT_TIMESTAMP)"
        ),
        [],
    )
    .map_err(|e| format!("grant role failed: {e}"))?;
    Ok(1)
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
                reject_reserved_schema(&schema)?;
                let relid = relation_oid(conn, &schema, &relation)?;
                Ok(("relation".to_string(), relid))
            })
            .collect(),
        GrantObjects::Schemas(names) => names
            .iter()
            .map(|name| {
                let schema = single_name_part(name)?;
                reject_reserved_schema(&schema)?;
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


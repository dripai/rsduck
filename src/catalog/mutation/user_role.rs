use super::*;

pub(in crate::catalog) fn create_user_account(
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
    let mysql_auth_string = mysql_caching_sha2_verifier(&password);

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
                "INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, mysql_auth_plugin, mysql_auth_string, status, is_builtin, created_at, updated_at, last_login_at) \
                 VALUES ({user_id}, '{}', '{}', 'argon2id', '{MYSQL_CACHING_SHA2_PASSWORD}', '{}', 'active', FALSE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)",
                sql_string(username),
                sql_string(&password_hash),
                sql_string(&mysql_auth_string)
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

pub(in crate::catalog) fn create_role_account(
    conn: &Connection,
    create_role: &CreateRole,
    sql: &str,
    _owner_user_id: i64,
) -> Result<usize, String> {
    if create_role.names.is_empty() {
        return Err("CREATE ROLE requires at least one role name".into());
    }
    if create_role.login.is_some()
        || create_role.inherit.is_some()
        || create_role.bypassrls.is_some()
        || create_role.password.is_some()
        || create_role.superuser.is_some()
        || create_role.create_db.is_some()
        || create_role.create_role.is_some()
        || create_role.replication.is_some()
        || create_role.connection_limit.is_some()
        || create_role.valid_until.is_some()
        || !create_role.in_role.is_empty()
        || !create_role.in_group.is_empty()
        || !create_role.role.is_empty()
        || !create_role.user.is_empty()
        || !create_role.admin.is_empty()
        || create_role.authorization_owner.is_some()
    {
        return Err(
            "CREATE ROLE only supports plain rsduck roles without extra role options".into(),
        );
    }

    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in &create_role.names {
            let role_name = single_name_part(name)?;
            validate_role_name(&role_name)?;
            if role_id_by_name_opt(conn, &role_name)?.is_some() {
                if create_role.if_not_exists {
                    continue;
                }
                return Err(format!("role already exists: {role_name}"));
            }
            let role_id = allocate_oid(conn)?;
            let journal_id = insert_journal(conn, "create_role", role_id, sql)?;
            conn.execute(
                &format!(
                    "INSERT INTO rsduck_catalog.rs_role(role_id, role_name, description, is_builtin, created_at, updated_at) \
                     VALUES ({role_id}, '{}', '', FALSE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                    sql_string(&role_name)
                ),
                [],
            )
            .map_err(|e| format!("write rs_role failed: {e}"))?;
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

pub(in crate::catalog) fn alter_user_account(
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
    let mysql_auth_string = mysql_caching_sha2_verifier(password);

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
                 SET password_hash = '{}', password_algo = 'argon2id', mysql_auth_plugin = '{MYSQL_CACHING_SHA2_PASSWORD}', mysql_auth_string = '{}', updated_at = CURRENT_TIMESTAMP \
                 WHERE user_id = {user_id}",
                sql_string(&password_hash),
                sql_string(&mysql_auth_string)
            ),
            [],
        )
        .map_err(|e| format!("update user password failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

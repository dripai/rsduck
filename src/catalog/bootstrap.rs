pub fn bootstrap_fresh(conn: &Connection) -> Result<(), String> {
    create_catalog_storage(conn)?;
    if catalog_version_row_exists(conn)? {
        return Ok(());
    }
    insert_bootstrap_rows(conn)
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

        for (role_id, action) in [
            (ROLE_ADMIN_ID, "manage_snapshot"),
            (ROLE_ADMIN_ID, "manage_catalog"),
            (ROLE_ADMIN_ID, "manage_user"),
            (ROLE_OPERATOR_ID, "manage_snapshot"),
            (ROLE_OPERATOR_ID, "manage_catalog"),
        ] {
            let privilege_id = allocate_oid(conn)?;
            conn.execute(
                &format!(
                    "INSERT INTO rsduck_catalog.rs_privilege(privilege_id, principal_type, principal_id, object_type, object_id, action, granted_by, created_at) \
                     VALUES ({privilege_id}, 'role', {role_id}, 'system', 0, '{}', {ADMIN_USER_ID}, CURRENT_TIMESTAMP)",
                    sql_string(action)
                ),
                [],
            )
            .map_err(|e| format!("write builtin system privilege failed: {e}"))?;
        }

        refresh_catalog_checksum(conn)?;
        Ok(0)
    })?;
    Ok(())
}


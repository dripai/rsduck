use super::*;

pub fn bootstrap_fresh(conn: &Connection) -> Result<(), String> {
    create_catalog_storage(conn)?;
    if catalog_version_row_exists(conn)? {
        return Ok(());
    }
    insert_bootstrap_rows(conn)
}

pub(super) fn insert_bootstrap_rows(conn: &Connection) -> Result<(), String> {
    let admin_password_hash = hash_password("admin")?;
    let admin_mysql_auth_string = mysql_caching_sha2_verifier("admin");
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
            &format!(
                "INSERT INTO rsduck_catalog.rs_catalog_version(id, schema_version, snapshot_format_version, catalog_epoch, catalog_checksum, status, created_at, updated_at) \
                 VALUES (1, {CATALOG_VERSION}, {SNAPSHOT_FORMAT_VERSION}, 0, '', 'ready', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
            ),
            [],
        )
        .map_err(|e| format!("write catalog version failed: {e}"))?;

        conn.execute_batch(&format!(
            "
            INSERT INTO rsduck_catalog.rs_schema VALUES
              ({INFORMATION_SCHEMA_NS}, 'information_schema', {ADMIN_USER_ID}, ''),
              ({RSDUCK_CATALOG_NS}, 'rsduck_catalog', {ADMIN_USER_ID}, ''),
              ({RSDUCK_INTERNAL_NS}, 'rsduck_internal', {ADMIN_USER_ID}, ''),
              ({MAIN_NS}, 'main', {ADMIN_USER_ID}, '');

            INSERT INTO rsduck_catalog.rs_type(oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type) VALUES
              ({TYPE_BOOL}, 'boolean', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 1, TRUE, 'b', 'B', TRUE, 0, 0, 0, 'BOOLEAN'),
              ({TYPE_INT8}, 'bigint', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'BIGINT'),
              ({TYPE_INT2}, 'smallint', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 2, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'SMALLINT'),
              ({TYPE_INT4}, 'integer', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'INTEGER'),
              ({TYPE_TEXT}, 'text', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'S', TRUE, 0, 0, 0, 'VARCHAR'),
              ({TYPE_FLOAT4}, 'real', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'REAL'),
              ({TYPE_FLOAT8}, 'double', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'N', TRUE, 0, 0, 0, 'DOUBLE'),
              ({TYPE_VARCHAR}, 'varchar', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'S', TRUE, 0, 0, 0, 'VARCHAR'),
              ({TYPE_DATE}, 'date', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 4, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'DATE'),
              ({TYPE_TIME}, 'time', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'TIME'),
              ({TYPE_TIMESTAMP}, 'timestamp', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, 8, TRUE, 'b', 'D', TRUE, 0, 0, 0, 'TIMESTAMP'),
              ({TYPE_NUMERIC}, 'numeric', {RSDUCK_CATALOG_NS}, {ADMIN_USER_ID}, -1, FALSE, 'b', 'N', TRUE, 0, 0, 0, 'DECIMAL');

            INSERT INTO rsduck_catalog.rs_role VALUES
              ({ROLE_ADMIN_ID}, 'admin', 'full catalog and system administration', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_OPERATOR_ID}, 'operator', 'snapshot and catalog operations', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_DDL_ID}, 'ddl', 'schema and relation ddl operations', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_WRITER_ID}, 'writer', 'relation data writes', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP),
              ({ROLE_READER_ID}, 'reader', 'relation reads', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP);

            INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, mysql_auth_plugin, mysql_auth_string, status, is_builtin, created_at, updated_at, last_login_at)
              VALUES ({ADMIN_USER_ID}, 'admin', '{}', 'argon2id', '{MYSQL_CACHING_SHA2_PASSWORD}', '{}', 'active', TRUE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL);

            INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at)
              VALUES ({ADMIN_USER_ID}, {ROLE_ADMIN_ID}, {ADMIN_USER_ID}, CURRENT_TIMESTAMP);
            ",
            sql_string(&admin_password_hash),
            sql_string(&admin_mysql_auth_string)
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

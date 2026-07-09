use super::{
    allocate_oid, authorize_snapshot, authorize_sql, bootstrap_fresh, evaluate_privilege_function,
    execute_catalog_aware_write, execute_catalog_aware_write_as, hash_password, namespace_oid,
    relation_oid, sql_string, validate_after_start, verify_password, CatalogAuthenticator,
    PG_CLASS_CLASSOID,
};
use crate::auth::{AuthCredential, AuthProtocol, AuthRequest, BlockingAuthenticator};
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

    let operator_system_privilege_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.rs_privilege p \
             JOIN rsduck_catalog.rs_role r ON r.role_id = p.principal_id \
             WHERE p.principal_type = 'role' \
               AND p.object_type = 'system' \
               AND p.object_id = 0 \
               AND r.role_name = 'operator' \
               AND p.action IN ('manage_snapshot', 'manage_catalog')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(operator_system_privilege_count, 2);

    let checksum: String = conn
        .query_row(
            "SELECT catalog_checksum FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(checksum.starts_with("fnv1a64:"));
}

#[test]
fn authenticate_default_admin_uses_catalog_password_hash() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let user_id = super::authenticate_user(&conn, "admin", "admin").unwrap();
    assert_eq!(user_id, 10);

    let login_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_user WHERE username = 'admin' AND last_login_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(login_count, 1);

    let err = super::authenticate_user(&conn, "admin", "wrong").unwrap_err();
    assert!(err.contains("invalid username or password"));
}

#[test]
fn catalog_authenticator_uses_protocol_neutral_request() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let authenticator = CatalogAuthenticator;
    let principal = authenticator
        .authenticate(
            &conn,
            &AuthRequest::cleartext(AuthProtocol::PgWire, "admin", "admin"),
        )
        .unwrap();
    assert_eq!(principal.user_id, 10);
    assert_eq!(principal.username, "admin");

    let err = authenticator
        .authenticate(
            &conn,
            &AuthRequest::cleartext(AuthProtocol::MySqlWire, "admin", "admin"),
        )
        .unwrap_err();
    assert_eq!(err, "invalid username or password");

    let err = authenticator
        .authenticate(
            &conn,
            &AuthRequest {
                protocol: AuthProtocol::MySqlWire,
                username: "admin".to_string(),
                credential: AuthCredential::MySqlNativePassword {
                    nonce: vec![1, 2, 3],
                    response: vec![4, 5, 6],
                },
            },
        )
        .unwrap_err();
    assert_eq!(err, "invalid username or password");
}

#[test]
fn authentication_failures_use_uniform_error() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let missing = super::authenticate_user(&conn, "missing_user", "pw").unwrap_err();
    assert_eq!(missing, "invalid username or password");

    execute_catalog_aware_write(&conn, "CREATE USER disabled_user PASSWORD='pw'").unwrap();
    conn.execute(
        "UPDATE rsduck_catalog.rs_user SET status = 'disabled' WHERE username = 'disabled_user'",
        [],
    )
    .unwrap();
    let disabled = super::authenticate_user(&conn, "disabled_user", "pw").unwrap_err();
    assert_eq!(disabled, "invalid username or password");

    execute_catalog_aware_write(&conn, "CREATE USER locked_user PASSWORD='pw'").unwrap();
    conn.execute(
        "UPDATE rsduck_catalog.rs_user SET status = 'locked' WHERE username = 'locked_user'",
        [],
    )
    .unwrap();
    let locked = super::authenticate_user(&conn, "locked_user", "pw").unwrap_err();
    assert_eq!(locked, "invalid username or password");

    execute_catalog_aware_write(&conn, "CREATE USER legacy_user PASSWORD='pw'").unwrap();
    conn.execute(
        "UPDATE rsduck_catalog.rs_user SET password_algo = 'legacy' WHERE username = 'legacy_user'",
        [],
    )
    .unwrap();
    let legacy = super::authenticate_user(&conn, "legacy_user", "pw").unwrap_err();
    assert_eq!(legacy, "invalid username or password");
}

#[test]
fn relation_permissions_are_enforced_for_non_admin_users() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
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
fn database_privilege_function_uses_system_privileges() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    insert_test_user(&conn, 106, "plain_db").unwrap();
    insert_test_user(&conn, 107, "catalog_db").unwrap();
    insert_test_user(&conn, 108, "operator_db").unwrap();

    let (_, plain_create) = evaluate_privilege_function(
        &conn,
        "plain_db",
        "SELECT has_database_privilege('postgres', 'CREATE')",
    )
    .unwrap();
    assert!(!plain_create);

    let (_, connect) = evaluate_privilege_function(
        &conn,
        "plain_db",
        "SELECT has_database_privilege('postgres', 'CONNECT')",
    )
    .unwrap();
    assert!(connect);

    insert_test_privilege(&conn, 107, "system", 0, "manage_catalog").unwrap();
    let (_, catalog_create) = evaluate_privilege_function(
        &conn,
        "catalog_db",
        "SELECT has_database_privilege('catalog_db', 'postgres', 'CREATE')",
    )
    .unwrap();
    assert!(catalog_create);

    conn.execute(
        "INSERT INTO rsduck_catalog.rs_user_role(user_id, role_id, granted_by, created_at) \
         VALUES (108, 21, 10, CURRENT_TIMESTAMP)",
        [],
    )
    .unwrap();
    let (_, operator_create) = evaluate_privilege_function(
        &conn,
        "operator_db",
        "SELECT has_database_privilege('operator_db', 'postgres', 'CREATE')",
    )
    .unwrap();
    assert!(operator_create);

    let (_, unknown_db) = evaluate_privilege_function(
        &conn,
        "catalog_db",
        "SELECT has_database_privilege('other_db', 'CREATE')",
    )
    .unwrap();
    assert!(!unknown_db);
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
fn user_management_mutations_update_authentication_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'").unwrap();
    assert!(super::authenticate_user(&conn, "alice", "pw").is_ok());

    let role_name: String = conn
        .query_row(
            "SELECT r.role_name \
             FROM rsduck_catalog.rs_user u \
             JOIN rsduck_catalog.rs_user_role ur ON ur.user_id = u.user_id \
             JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id \
             WHERE u.username = 'alice'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_name, "reader");

    let denied = authorize_sql(&conn, "alice", "CREATE USER bob PASSWORD='pw'").unwrap_err();
    assert!(denied.contains("manage_user"));

    execute_catalog_aware_write(&conn, "ALTER USER alice PASSWORD 'newpw'").unwrap();
    assert!(super::authenticate_user(&conn, "alice", "pw").is_err());
    assert!(super::authenticate_user(&conn, "alice", "newpw").is_ok());

    execute_catalog_aware_write(&conn, "DROP USER alice").unwrap();
    assert!(super::authenticate_user(&conn, "alice", "newpw").is_err());
}

#[test]
fn role_management_mutations_update_user_role_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)").unwrap();
    execute_catalog_aware_write(&conn, "CREATE USER bob PASSWORD='pw'").unwrap();
    execute_catalog_aware_write(&conn, "CREATE ROLE analyst").unwrap();

    let role_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_role WHERE role_name = 'analyst'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(role_count, 1);

    execute_catalog_aware_write(&conn, "GRANT SELECT ON TABLE quotes TO ROLE analyst").unwrap();
    execute_catalog_aware_write(&conn, "GRANT ROLE analyst TO bob").unwrap();
    authorize_sql(&conn, "bob", "SELECT * FROM quotes").unwrap();

    execute_catalog_aware_write(&conn, "REVOKE ROLE analyst FROM bob").unwrap();
    let denied = authorize_sql(&conn, "bob", "SELECT * FROM quotes").unwrap_err();
    assert!(denied.contains("permission denied"));

    let drop_err = execute_catalog_aware_write(&conn, "DROP ROLE analyst").unwrap_err();
    assert!(drop_err.contains("revoke grants first"));
    execute_catalog_aware_write(&conn, "REVOKE SELECT ON TABLE quotes FROM ROLE analyst").unwrap();
    execute_catalog_aware_write(&conn, "DROP ROLE analyst").unwrap();

    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_role WHERE role_name = 'analyst'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    validate_after_start(&conn).unwrap();
}

#[test]
fn grant_and_revoke_relation_and_schema_privileges() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
    execute_catalog_aware_write(&conn, "CREATE USER reader_user PASSWORD='pw'").unwrap();
    execute_catalog_aware_write(&conn, "CREATE USER ddl_user PASSWORD='pw'").unwrap();

    let denied = authorize_sql(&conn, "reader_user", "SELECT * FROM quotes").unwrap_err();
    assert!(denied.contains("permission denied"));

    execute_catalog_aware_write(&conn, "GRANT SELECT ON TABLE quotes TO reader_user").unwrap();
    authorize_sql(&conn, "reader_user", "SELECT * FROM quotes").unwrap();

    execute_catalog_aware_write(&conn, "GRANT INSERT ON TABLE quotes TO reader_user").unwrap();
    authorize_sql(&conn, "reader_user", "INSERT INTO quotes VALUES ('A', 1.0)").unwrap();

    execute_catalog_aware_write(&conn, "REVOKE SELECT ON TABLE quotes FROM reader_user").unwrap();
    let denied = authorize_sql(&conn, "reader_user", "SELECT * FROM quotes").unwrap_err();
    assert!(denied.contains("permission denied"));

    execute_catalog_aware_write(&conn, "GRANT CREATE ON SCHEMA main TO ddl_user").unwrap();
    authorize_sql(
        &conn,
        "ddl_user",
        "CREATE TABLE created_by_grant(id INTEGER)",
    )
    .unwrap();
}

#[test]
fn grant_on_reserved_schema_is_rejected_without_privilege_rows() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE USER reader_user PASSWORD='pw'").unwrap();

    let err = execute_catalog_aware_write(
        &conn,
        "GRANT CREATE ON SCHEMA rsduck_catalog TO reader_user",
    )
    .unwrap_err();
    assert_eq!(
        err,
        "reserved schema is managed by rsduck catalog: rsduck_catalog"
    );

    let reserved_privilege_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.rs_privilege p \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = p.object_id \
             WHERE p.object_type = 'schema' \
               AND n.nspname IN ('pg_catalog', 'information_schema', 'rsduck_catalog', 'rsduck_internal')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reserved_privilege_count, 0);
}

#[test]
fn create_table_writes_pg_class_and_attributes() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    let before_checksum: String = conn
        .query_row(
            "SELECT catalog_checksum FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE kline_day(code VARCHAR NOT NULL, bar_time TIMESTAMP NOT NULL, close DOUBLE, PRIMARY KEY(code, bar_time))",
    )
    .unwrap();
    let after_checksum: String = conn
        .query_row(
            "SELECT catalog_checksum FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(before_checksum, after_checksum);

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
fn create_table_rejects_unsupported_duckdb_type_without_leftovers() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let err =
        execute_catalog_aware_write(&conn, "CREATE TABLE bad_metric(flag TINYINT)").unwrap_err();
    assert!(err.contains("unsupported DuckDB type"));
    assert!(err.contains("TINYINT"));

    let catalog_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname = 'bad_metric'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(catalog_count, 0);

    let physical_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables() WHERE schema_name = 'main' AND table_name = 'bad_metric'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(physical_count, 0);
}

#[test]
fn create_partitioned_table_rejects_unsupported_type_without_leftovers() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let err = execute_catalog_aware_write(
        &conn,
        "CREATE TABLE bad_metric(id BIGINT, access_time TIMESTAMP NOT NULL, flag TINYINT)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap_err();
    assert!(err.contains("unsupported DuckDB type"));
    assert!(err.contains("TINYINT"));

    let catalog_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname LIKE 'bad_metric%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(catalog_count, 0);

    let physical_table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables() \
             WHERE table_name IN ('bad_metric', 'bad_metric_null')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(physical_table_count, 0);
    let physical_view_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_views() WHERE view_name = 'bad_metric'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(physical_view_count, 0);
}

#[test]
fn create_table_foreign_key_writes_constraint_and_dependencies() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE instruments(code VARCHAR PRIMARY KEY)")
        .unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE quotes(
            code VARCHAR,
            close DOUBLE,
            CONSTRAINT fk_quotes_instruments FOREIGN KEY(code) REFERENCES instruments(code)
        )",
    )
    .unwrap();

    let (contype, conkey, confrelid, confkey): (String, String, i64, String) = conn
        .query_row(
            "SELECT contype, conkey, confrelid, confkey \
             FROM rsduck_catalog.pg_constraint \
             WHERE conname = 'fk_quotes_instruments'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(contype, "f");
    assert_eq!(conkey, "1");
    assert_eq!(confkey, "1");
    assert!(confrelid > 0);

    let referenced_column_dependency_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             JOIN rsduck_catalog.pg_constraint con ON con.oid = d.objid \
             JOIN rsduck_catalog.pg_class c ON c.oid = d.refobjid \
             WHERE con.conname = 'fk_quotes_instruments' \
               AND c.relname = 'instruments' \
               AND d.refobjsubid = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(referenced_column_dependency_count, 1);

    let drop_err = execute_catalog_aware_write(&conn, "DROP TABLE instruments").unwrap_err();
    assert!(drop_err.contains("dependent objects"));

    validate_after_start(&conn).unwrap();
}

#[test]
fn create_view_and_index_write_catalog_metadata() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
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

    let view_dependency_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             JOIN rsduck_catalog.pg_class view_rel ON view_rel.oid = d.objid \
             JOIN rsduck_catalog.pg_class table_rel ON table_rel.oid = d.refobjid \
             WHERE view_rel.relname = 'quote_view' \
               AND table_rel.relname = 'quotes' \
               AND d.classid = 1259 \
               AND d.refclassid = 1259",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(view_dependency_count, 1);

    let drop_err = execute_catalog_aware_write(&conn, "DROP TABLE quotes").unwrap_err();
    assert!(drop_err.contains("dependent objects"));

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
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();

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
fn alter_table_drop_column_marks_catalog_column_dropped() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE quotes(code VARCHAR, close DOUBLE, source TEXT DEFAULT 'web')",
    )
    .unwrap();

    execute_catalog_aware_write(&conn, "ALTER TABLE quotes DROP COLUMN close").unwrap();

    assert!(conn.prepare("SELECT close FROM quotes").is_err());
    let dropped: bool = conn
        .query_row(
            "SELECT a.attisdropped \
             FROM rsduck_catalog.pg_attribute a \
             JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'quotes' AND a.attname = 'close'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(dropped);
    let active_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_attribute a \
             JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'quotes' AND a.attisdropped = FALSE",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_count, 2);

    execute_catalog_aware_write(&conn, "ALTER TABLE quotes ADD COLUMN venue TEXT").unwrap();
    let (close_attnum, venue_attnum): (i32, i32) = conn
        .query_row(
            "SELECT \
               MAX(CASE WHEN a.attname = 'close' THEN a.attnum ELSE NULL END), \
               MAX(CASE WHEN a.attname = 'venue' THEN a.attnum ELSE NULL END) \
             FROM rsduck_catalog.pg_attribute a \
             JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'quotes'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(venue_attnum > close_attnum);
}

#[test]
fn comment_on_table_and_column_writes_pg_description() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();

    execute_catalog_aware_write(&conn, "COMMENT ON TABLE quotes IS 'quotes table'").unwrap();
    execute_catalog_aware_write(&conn, "COMMENT ON COLUMN quotes.close IS 'close price'").unwrap();

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
fn comment_on_reserved_schema_is_rejected() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let err = execute_catalog_aware_write(
        &conn,
        "COMMENT ON SCHEMA rsduck_catalog IS 'internal catalog'",
    )
    .unwrap_err();
    assert_eq!(
        err,
        "reserved schema is managed by rsduck catalog: rsduck_catalog"
    );

    let comment_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_description d \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = d.objoid \
             WHERE n.nspname = 'rsduck_catalog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(comment_count, 0);
}

#[test]
fn drop_index_and_table_updates_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
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
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
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
fn create_managed_partitioned_table_creates_empty_entrypoint_without_partitions() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            user_id VARCHAR(64),
            access_time TIMESTAMP NOT NULL,
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

    let partition_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_partition",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(partition_count, 0);

    let visible_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 0);
}

#[test]
fn insert_into_partitioned_table_creates_partitions_and_rejects_dirty_rows() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            user_id VARCHAR(64),
            access_time TIMESTAMP NOT NULL,
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
         (2, 'u2', '2026-07-02 08:30:00', 'ok-2')",
    )
    .unwrap();
    assert_eq!(affected, Some(2));

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
    assert_eq!(partition_count, 2);

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
    assert_eq!(visible_rows, 2);

    let null_key_err = execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, user_id, access_time, content) VALUES
         (3, 'u3', NULL, 'null-key')",
    )
    .unwrap_err();
    assert!(null_key_err.contains("partition key value is NULL or cannot be routed"));
    let dirty_key_err = execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, user_id, access_time, content) VALUES
         (4, 'u4', 'bad-time', 'dirty-key')",
    )
    .unwrap_err();
    assert!(dirty_key_err.contains("partition key value is NULL or cannot be routed"));

    let july_1_count: i64 = conn
        .query_row(
            "SELECT row_count FROM rsduck_catalog.rs_partition WHERE partition_value = '20260701'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(july_1_count, 1);
}

#[test]
fn partition_retention_expires_old_ordinary_partitions() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            access_time TIMESTAMP NOT NULL,
            content TEXT
         )
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '2')",
    )
    .unwrap();

    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content) VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00', 'old'),
         (2, TIMESTAMP '2026-07-02 10:00:00', 'keep-1'),
         (3, TIMESTAMP '2026-07-03 10:00:00', 'keep-2')",
    )
    .unwrap();

    let active_ordinary_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_partition \
             WHERE status = 'active' AND is_null_partition = FALSE",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active_ordinary_count, 2);

    let dropped_old_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_partition \
             WHERE partition_value = '20260701' AND status = 'dropped' AND dropped_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dropped_old_count, 1);

    let old_physical_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables() \
             WHERE schema_name = 'rsduck_internal' AND table_name = 'ods_access_log_20260701'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_physical_count, 0);

    let visible_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 2);

    let pg_class_old_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_class WHERE relname = 'ods_access_log_20260701'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pg_class_old_count, 0);

    let recreate_err = execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content) VALUES
         (5, TIMESTAMP '2026-07-01 11:00:00', 'old-again')",
    )
    .unwrap_err();
    assert!(recreate_err.contains("non-active status"));
    assert!(recreate_err.contains("explicit repair or retry"));
}

#[test]
fn insert_select_into_partitioned_table_routes_rows() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE src_access_log(id BIGINT, access_time TIMESTAMP NOT NULL, content TEXT)",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL, content TEXT)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO src_access_log VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00', 'ok-1'),
         (2, TIMESTAMP '2026-07-02 10:00:00', 'ok-2')",
        [],
    )
    .unwrap();

    let affected = execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content)
         SELECT id, access_time, content FROM src_access_log",
    )
    .unwrap();
    assert_eq!(affected, Some(2));

    let visible_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 2);
    let ordinary_partitions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_partition
             WHERE is_null_partition = FALSE AND status = 'active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ordinary_partitions, 2);
}

#[test]
fn partitioned_table_accepts_table_constraints_in_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(
            id BIGINT,
            access_time TIMESTAMP NOT NULL,
            content TEXT,
            CONSTRAINT ods_access_log_id_key UNIQUE(id),
            CONSTRAINT ods_access_log_id_check CHECK (id > 0)
         )
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();

    let parent_oid = relation_oid(&conn, "main", "ods_access_log").unwrap();
    let constraint_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_constraint WHERE conrelid = {parent_oid}"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(constraint_count, 2);

    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content)
         VALUES (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
    )
    .unwrap();
    let physical_partition_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables()
             WHERE schema_name = 'rsduck_internal'
               AND table_name = 'ods_access_log_20260701'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(physical_partition_count, 1);
}

#[test]
fn partitioned_index_creates_indexes_for_existing_and_future_partitions() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL, content TEXT)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content)
         VALUES (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
    )
    .unwrap();

    execute_catalog_aware_write(
        &conn,
        "CREATE INDEX idx_ods_access_log_time ON ods_access_log(access_time)",
    )
    .unwrap();

    let parent_index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_index i
             JOIN rsduck_catalog.pg_class c ON c.oid = i.indexrelid
             WHERE c.relname = 'idx_ods_access_log_time'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent_index_count, 1);
    let initial_physical_index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_indexes()
             WHERE schema_name = 'rsduck_internal'
               AND index_name IN (
                'ods_access_log_20260701__idx_ods_access_log_time'
               )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(initial_physical_index_count, 1);

    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content)
         VALUES (3, TIMESTAMP '2026-07-02 10:00:00', 'ok-2')",
    )
    .unwrap();
    let future_index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_indexes()
             WHERE schema_name = 'rsduck_internal'
               AND index_name = 'ods_access_log_20260702__idx_ods_access_log_time'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(future_index_count, 1);
}

#[test]
fn partition_management_calls_are_catalog_authorized_mutations() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL, content TEXT)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content)
         VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
    )
    .unwrap();

    execute_catalog_aware_write(
        &conn,
        "CALL rsduck_mark_partition_unavailable('ods_access_log', '20260701', 'manual test')",
    )
    .unwrap();
    let failed_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_partition
             WHERE partition_value = '20260701' AND status = 'failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_count, 1);
    let visible_rows_after_mark: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows_after_mark, 0);

    execute_catalog_aware_write(
        &conn,
        "CALL rsduck_repair_partition('ods_access_log', '20260701')",
    )
    .unwrap();
    let visible_rows_after_repair: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows_after_repair, 1);

    execute_catalog_aware_write(&conn, "CALL rsduck_run_partition_maintenance()").unwrap();
}

#[test]
fn new_partition_preserves_parent_column_defaults() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            access_time TIMESTAMP NOT NULL,
            source TEXT DEFAULT 'web'
         )
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();

    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time) VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00')",
    )
    .unwrap();

    let source: String = conn
        .query_row(
            "SELECT source FROM ods_access_log WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, "web");

    let physical_default_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_attrdef d \
             JOIN rsduck_catalog.pg_class c ON c.oid = d.adrelid \
             JOIN rsduck_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.adnum \
             WHERE c.relname = 'ods_access_log_20260701' \
               AND a.attname = 'source' \
               AND d.adbin = '''web'''",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(physical_default_count, 1);
}

#[test]
fn alter_partitioned_table_add_column_updates_parent_and_partitions() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            access_time TIMESTAMP NOT NULL,
            content TEXT
         )
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content) VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
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
             WHERE c.relname = 'ods_access_log_20260701' \
               AND a.attname = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(child_attr_count, 1);

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
fn alter_partitioned_table_drop_column_updates_parent_partitions_and_entrypoint() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log (
            id BIGINT,
            access_time TIMESTAMP NOT NULL,
            content TEXT
         )
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time, content) VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
    )
    .unwrap();

    execute_catalog_aware_write(&conn, "ALTER TABLE ods_access_log DROP COLUMN content").unwrap();

    let visible_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ods_access_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_rows, 1);
    assert!(conn.prepare("SELECT content FROM ods_access_log").is_err());
    let dropped_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_attribute a \
             JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
             WHERE c.relname IN ('ods_access_log', 'ods_access_log_20260701') \
               AND a.attname = 'content' \
               AND a.attisdropped = TRUE",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dropped_count, 2);

    let err =
        execute_catalog_aware_write(&conn, "ALTER TABLE ods_access_log DROP COLUMN access_time")
            .unwrap_err();
    assert!(err.contains("partition key"));
}

#[test]
fn drop_partitioned_table_removes_entrypoint_partitions_and_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time) VALUES
         (1, TIMESTAMP '2026-07-01 10:00:00')",
    )
    .unwrap();

    execute_catalog_aware_write(&conn, "DROP TABLE ods_access_log").unwrap();

    let class_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_class \
             WHERE relname IN ('ods_access_log', 'ods_access_log_20260701')",
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
               AND table_name = 'ods_access_log_20260701'",
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
        "CREATE TABLE bad_hour(id BIGINT, trade_date DATE NOT NULL)
         PARTITION BY RANGE (trade_date)
         WITH (partition_unit = 'hour', retention = '7')",
    )
    .unwrap_err();
    assert!(err.contains("DATE partition key does not support"));

    let err = execute_catalog_aware_write(
        &conn,
        "CREATE TABLE bad_nullable(id BIGINT, access_time TIMESTAMP)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '7')",
    )
    .unwrap_err();
    assert!(err.contains("must be NOT NULL"));
}

#[test]
fn startup_validation_rebuilds_partition_entrypoint_from_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
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
fn startup_validation_rebuilds_partition_dependencies_from_catalog() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    let parent_oid: i64 = conn
        .query_row(
            "SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'ods_access_log'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_partition_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_partition \
                 WHERE parent_relid = {parent_oid} AND status = 'active'"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.pg_depend \
             WHERE classid = {PG_CLASS_CLASSOID} AND objid = {parent_oid} \
               AND refclassid = {PG_CLASS_CLASSOID}"
        ),
        [],
    )
    .unwrap();
    super::refresh_catalog_checksum(&conn).unwrap();

    validate_after_start(&conn).unwrap();

    let dependency_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_depend \
                 WHERE classid = {PG_CLASS_CLASSOID} AND objid = {parent_oid} \
                   AND refclassid = {PG_CLASS_CLASSOID}"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dependency_count, active_partition_count);
}

#[test]
fn startup_validation_marks_partition_parent_unavailable_when_child_missing() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(
        &conn,
        "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
         PARTITION BY RANGE (access_time)
         WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();
    execute_catalog_aware_write(
        &conn,
        "INSERT INTO ods_access_log(id, access_time)
         VALUES (1, TIMESTAMP '2026-07-01 10:00:00')",
    )
    .unwrap();
    conn.execute("DROP VIEW ods_access_log", []).unwrap();
    conn.execute("DROP TABLE rsduck_internal.ods_access_log_20260701", [])
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
            "SELECT status FROM rsduck_catalog.rs_partition WHERE partition_value = '20260701'",
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
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
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

    let err = authorize_sql(&conn, "admin", "SELECT * FROM quotes").unwrap_err();
    assert!(err.contains("relation is unavailable"));
    assert!(err.contains("RS-CATALOG-"));
    assert!(err.contains("missing DuckDB physical table"));

    let projection_sql =
        crate::pg_compat::rewrite_sql("SELECT relname, rsduck_status, rsduck_error_message FROM pg_catalog.pg_class WHERE relname = 'quotes'")
            .expect("rewrite pg_class projection");
    let (relname, projected_status, projected_error): (String, String, String) = conn
        .query_row(&projection_sql, [], |row| {
            Ok((
                row.get("relname")?,
                row.get("rsduck_status")?,
                row.get("rsduck_error_message")?,
            ))
        })
        .unwrap();
    assert_eq!(relname, "quotes");
    assert_eq!(projected_status, "unavailable");
    assert!(projected_error.contains("RS-CATALOG-"));
    assert!(projected_error.contains("missing DuckDB physical table"));
}

#[test]
fn startup_validation_marks_column_order_mismatch_unavailable() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
    conn.execute("DROP TABLE quotes", []).unwrap();
    conn.execute("CREATE TABLE quotes(close DOUBLE, code VARCHAR)", [])
        .unwrap();

    validate_after_start(&conn).unwrap();

    let (status, error_message): (String, String) = conn
        .query_row(
            "SELECT status, error_message FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "unavailable");
    assert!(error_message.contains("RS-CATALOG-"));
    assert!(error_message.contains("column mismatch"));

    let err = authorize_sql(&conn, "admin", "SELECT * FROM quotes").unwrap_err();
    assert!(err.contains("main.quotes"));
    assert!(err.contains("RS-CATALOG-"));
    assert!(err.contains("column mismatch"));
}

#[test]
fn startup_validation_recovers_unfinished_catalog_journal() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    conn.execute(
        "INSERT INTO rsduck_catalog.rs_catalog_journal(
            journal_id, catalog_epoch, mutation_type, target_oid, request_json,
            status, error_message, created_at, applied_at
         )
         VALUES (999, 0, 'create_table', 123, '{}', 'pending', '', CURRENT_TIMESTAMP, NULL)",
        [],
    )
    .unwrap();

    validate_after_start(&conn).unwrap();
    let (status, error_message): (String, String) = conn
        .query_row(
            "SELECT status, error_message \
             FROM rsduck_catalog.rs_catalog_journal \
             WHERE journal_id = 999",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert!(error_message.contains("recovered at startup"));
    assert!(error_message.contains("create_table"));
}

#[test]
fn startup_validation_rejects_broken_catalog_references() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR, close DOUBLE)").unwrap();
    conn.execute(
        "UPDATE rsduck_catalog.pg_attribute \
         SET atttypid = 999999 \
         WHERE attname = 'code'",
        [],
    )
    .unwrap();

    let err = validate_after_start(&conn).unwrap_err();
    assert!(err.contains("pg_attribute.atttypid"));
}

#[test]
fn startup_validation_rejects_catalog_checksum_mismatch() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();
    conn.execute(
        "UPDATE rsduck_catalog.pg_namespace \
         SET nspacl = 'tampered' \
         WHERE nspname = 'main'",
        [],
    )
    .unwrap();

    let err = validate_after_start(&conn).unwrap_err();
    assert!(err.contains("catalog checksum mismatch"));
}

#[test]
fn reserved_schema_write_is_rejected() {
    let conn = Connection::open_in_memory().unwrap();
    bootstrap_fresh(&conn).unwrap();

    let err =
        execute_catalog_aware_write(&conn, "CREATE TABLE rsduck_catalog.bad_table(id INTEGER)")
            .unwrap_err();
    assert!(err.contains("reserved schema"));

    let err = super::guard_external_sql_as("admin", "SELECT * FROM rsduck_internal.bad_table")
        .unwrap_err();
    assert_eq!(
        err,
        "reserved schema is managed by rsduck catalog: rsduck_internal"
    );
}

fn insert_test_user(conn: &Connection, user_id: i64, username: &str) -> Result<(), String> {
    let password_hash = hash_password("pw")?;
    let mysql_auth_string = super::mysql_caching_sha2_verifier("pw");
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_user(user_id, username, password_hash, password_algo, mysql_auth_plugin, mysql_auth_string, status, is_builtin, created_at, updated_at, last_login_at) \
             VALUES ({user_id}, '{}', '{}', 'argon2id', '{}', '{}', 'active', FALSE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)",
            sql_string(username),
            sql_string(&password_hash),
            super::MYSQL_CACHING_SHA2_PASSWORD,
            sql_string(&mysql_auth_string)
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

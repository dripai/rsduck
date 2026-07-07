mod tests {
    use super::{
        execute_sql_blocking, export_database_sql, find_latest_snapshot_dir, import_database_sql,
        parse_snapshot_dir_timestamp, restore_or_initialize, save_snapshot_blocking, SqlResult,
        SNAPSHOT_MANIFEST_FILE,
    };
    use crate::sql_route::route_sql;
    use duckdb::Connection;
    use std::path::PathBuf;

    #[test]
    fn parse_snapshot_dir_timestamp_only_accepts_final_snapshot_dirs() {
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500", "rsduck").is_some());
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500.tmp", "rsduck").is_none());
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500.parquet", "rsduck").is_none());
        assert!(parse_snapshot_dir_timestamp("rsduck_latest", "rsduck").is_none());
    }

    #[test]
    fn find_latest_snapshot_dir_uses_newest_final_snapshot_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_test_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let dirs = [
            "rsduck_20260702_101500",
            "rsduck_20260702_101700.tmp",
            "rsduck_20260702_101600",
            "other_20260702_101900",
        ];
        for dir_name in dirs {
            std::fs::create_dir_all(dir.join(dir_name)).unwrap();
        }
        std::fs::write(dir.join("rsduck_20260702_101800.parquet"), b"").unwrap();
        std::fs::write(dir.join("rsduck_20260702_101900.parquet.tmp"), b"").unwrap();

        let latest = find_latest_snapshot_dir(dir.to_str().unwrap(), "rsduck").unwrap();
        assert_eq!(
            PathBuf::from(latest).file_name().unwrap().to_string_lossy(),
            "rsduck_20260702_101600"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catalog_projection_rewrite_executes_through_db_auth_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let sql = "SELECT relname FROM pg_catalog.pg_class WHERE relname = 'quotes'";
        let decision = route_sql(sql).unwrap();
        let result =
            execute_sql_blocking(&conn, "alice", sql, decision.route, &decision.command, 100)
                .unwrap();

        let SqlResult::Query { columns, rows } = result else {
            panic!("expected catalog projection query result");
        };
        let relname_idx = columns
            .iter()
            .position(|column| column == "relname")
            .expect("relname column");
        assert!(rows.iter().any(|row| row[relname_idx] == "quotes"));
    }

    #[test]
    fn internal_catalog_query_requires_catalog_diagnostic_privilege() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE USER operator_user PASSWORD='pw'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER plain_user PASSWORD='pw'")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "GRANT ROLE operator TO operator_user")
            .unwrap();

        let sql = "SELECT * FROM rsduck_catalog.pg_class";
        let decision = route_sql(sql).unwrap();
        execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100).unwrap();
        execute_sql_blocking(
            &conn,
            "operator_user",
            sql,
            decision.route,
            &decision.command,
            100,
        )
        .unwrap();
        let err = execute_sql_blocking(
            &conn,
            "plain_user",
            sql,
            decision.route,
            &decision.command,
            100,
        )
        .unwrap_err();
        assert!(err.contains("manage_catalog"));
    }

    #[test]
    fn reserved_pg_catalog_write_is_rejected_through_db_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        let sql = "INSERT INTO pg_catalog.pg_class VALUES (1)";
        let decision = route_sql(sql).unwrap();
        let err = execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap_err();
        assert_eq!(
            err,
            "reserved schema is managed by rsduck catalog: pg_catalog"
        );
    }

    #[test]
    fn unsupported_catalog_relation_reports_relation_name() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        let sql = "SELECT * FROM pg_catalog.pg_am";
        let decision = route_sql(sql).unwrap();
        let err = execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap_err();
        assert_eq!(err, "unsupported pg_catalog relation: pg_am");
    }

    #[test]
    fn snapshot_directory_round_trip_restores_multiple_tables() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_round_trip_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE table_a(id INTEGER, name VARCHAR)",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO table_a VALUES (1, 'alpha'), (2, 'beta');")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE table_b(id INTEGER, amount DOUBLE)",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO table_b VALUES (10, 1.5);")
            .unwrap();

        let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
        let (catalog_epoch, catalog_checksum): (i64, String) = conn
            .query_row(
                "SELECT catalog_epoch, catalog_checksum \
                 FROM rsduck_catalog.rs_catalog_version \
                 WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let manifest_path = PathBuf::from(&snapshot).join("rsduck_snapshot_manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["manifest_version"], 1);
        assert_eq!(manifest["catalog_epoch"], catalog_epoch);
        assert_eq!(manifest["catalog_checksum"], catalog_checksum);
        assert_eq!(
            manifest["snapshot_name"],
            PathBuf::from(&snapshot)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );

        let restored = Connection::open_in_memory().unwrap();
        restore_or_initialize(&restored, Some(&snapshot), "").unwrap();

        let table_a_count: i64 = restored
            .query_row("SELECT COUNT(*) FROM table_a", [], |row| row.get(0))
            .unwrap();
        let table_b_count: i64 = restored
            .query_row("SELECT COUNT(*) FROM table_b", [], |row| row.get(0))
            .unwrap();
        assert_eq!(table_a_count, 2);
        assert_eq!(table_b_count, 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_restore_rejects_manifest_checksum_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_manifest_mismatch_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE table_a(id INTEGER)")
            .unwrap();

        let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
        let manifest_path = PathBuf::from(&snapshot).join(SNAPSHOT_MANIFEST_FILE);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["catalog_checksum"] = serde_json::Value::String("tampered".to_string());
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let restored = Connection::open_in_memory().unwrap();
        let err = restore_or_initialize(&restored, Some(&snapshot), "").unwrap_err();
        assert!(err.contains("snapshot manifest catalog_checksum mismatch"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_and_import_sql_escape_paths() {
        assert_eq!(
            export_database_sql("snapshot/rsduck's.tmp"),
            "EXPORT DATABASE 'snapshot/rsduck''s.tmp' (FORMAT parquet, COMPRESSION zstd)"
        );
        assert_eq!(
            import_database_sql("snapshot/rsduck's"),
            "IMPORT DATABASE 'snapshot/rsduck''s'"
        );
    }
}

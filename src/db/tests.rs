use super::{
    describe_sql_blocking, execute_sql_blocking, execute_typed_sql_blocking, export_database_sql,
    find_latest_snapshot_dir, import_database_sql, parse_snapshot_dir_timestamp,
    reset_admin_password_offline, restore_or_initialize, save_snapshot_blocking, SqlParam,
    SqlResult, SqlType, SqlTypedResult, SqlValue, SNAPSHOT_MANIFEST_FILE,
};
use crate::sql_route::route_sql;
use duckdb::Connection;
use std::path::PathBuf;
use std::time::Duration;

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
        if !dir_name.ends_with(".tmp") {
            let manifest = serde_json::json!({
                "snapshot_format_version": 2,
                "snapshot_name": dir_name,
                "created_at": "2026-07-10T00:00:00+08:00",
                "catalog_epoch": 0,
                "catalog_checksum": "",
                "rsduck_version": "test",
                "tables": [],
                "partitions": [],
                "views": [],
                "macros": []
            });
            std::fs::write(
                dir.join(dir_name).join(SNAPSHOT_MANIFEST_FILE),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        }
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
fn describe_sql_reports_read_columns_without_executing_writes() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE TABLE quotes(code VARCHAR, price DOUBLE)",
    )
    .unwrap();

    let sql = "SELECT code, price FROM quotes";
    let decision = route_sql(sql).unwrap();
    let columns = describe_sql_blocking(&conn, "admin", sql, decision.route).unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        vec!["code", "price"]
    );

    let write_sql = "CREATE USER bob PASSWORD='pw'";
    let decision = route_sql(write_sql).unwrap();
    let columns = describe_sql_blocking(&conn, "admin", write_sql, decision.route).unwrap();
    assert!(columns.is_empty());
    assert!(crate::catalog::authenticate_user(&conn, "bob", "pw").is_err());
}

#[test]
fn comment_on_table_executes_through_web_sql_path() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
        .unwrap();

    let sql = "COMMENT ON TABLE quotes IS 'quotes table'";
    let decision = route_sql(sql).unwrap();
    assert_eq!(decision.command, "COMMENT");
    execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100).unwrap();

    let comment: String = conn
        .query_row(
            "SELECT description
             FROM rsduck_catalog.rs_comment d
             JOIN rsduck_catalog.rs_relation c ON d.objoid = c.oid
             WHERE c.relname = 'quotes' AND d.objsubid = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(comment, "quotes table");
}

#[test]
fn information_schema_hides_relations_without_read_privilege() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
        .unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER metadata_reader PASSWORD='pw'")
        .unwrap();

    let sql = "SELECT table_name FROM information_schema.tables WHERE table_schema = 'main'";
    let decision = route_sql(sql).unwrap();
    let hidden = execute_typed_sql_blocking(
        &conn,
        "metadata_reader",
        sql,
        decision.route,
        &decision.command,
        100,
    )
    .unwrap();
    let SqlTypedResult::Query { rows, .. } = hidden else {
        panic!("expected metadata query result");
    };
    assert!(rows.is_empty());

    crate::catalog::execute_catalog_aware_write(
        &conn,
        "GRANT SELECT ON TABLE quotes TO metadata_reader",
    )
    .unwrap();
    let visible = execute_typed_sql_blocking(
        &conn,
        "metadata_reader",
        sql,
        decision.route,
        &decision.command,
        100,
    )
    .unwrap();
    let SqlTypedResult::Query { rows, .. } = visible else {
        panic!("expected metadata query result");
    };
    assert_eq!(rows, vec![vec![SqlValue::Text("quotes".to_string())]]);
}

#[test]
fn information_schema_reports_catalog_and_duckdb_mismatch() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    conn.execute("CREATE TABLE untracked_physical_table(id INTEGER)", [])
        .unwrap();

    let sql = "SELECT table_name FROM information_schema.tables";
    let decision = route_sql(sql).unwrap();
    let err =
        execute_typed_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap_err();
    assert_eq!(
        err,
        "metadata projection inconsistent: DuckDB relation is missing from catalog: main.untracked_physical_table"
    );
}

#[test]
fn mysql_user_projection_requires_user_management_privilege() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER plain_reader PASSWORD='pw'")
        .unwrap();

    let sql = "SELECT user, host FROM mysql.user";
    let decision = route_sql(sql).unwrap();
    let err = execute_typed_sql_blocking(
        &conn,
        "plain_reader",
        sql,
        decision.route,
        &decision.command,
        100,
    )
    .unwrap_err();
    assert!(err.contains("manage_user"));
}

#[test]
fn typed_query_preserves_common_duckdb_types_and_null_cells() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();

    let sql = "\
        SELECT \
            CAST(1 AS INTEGER) AS id, \
            CAST(1.5 AS DOUBLE) AS price, \
            DATE '2026-07-09' AS trade_date, \
            '' AS empty_text, \
            CAST(NULL AS VARCHAR) AS missing_text";
    let decision = route_sql(sql).unwrap();
    let result =
        execute_typed_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap();

    let SqlTypedResult::Query { columns, rows } = result else {
        panic!("expected typed query result");
    };
    assert_eq!(
        columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type))
            .collect::<Vec<_>>(),
        vec![
            ("id", SqlType::Int4),
            ("price", SqlType::Float8),
            ("trade_date", SqlType::Date),
            ("empty_text", SqlType::Text),
            ("missing_text", SqlType::Text),
        ]
    );
    assert_eq!(
        rows,
        vec![vec![
            SqlValue::Int32(1),
            SqlValue::Float64(1.5),
            SqlValue::Date(chrono::NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()),
            SqlValue::Text(String::new()),
            SqlValue::Null,
        ]]
    );
}

#[test]
fn typed_query_preserves_complex_values_as_json() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();

    let sql = "\
        SELECT \
            [1, NULL, 2] AS items, \
            {'code': 'AAPL', 'price': 1.5, 'halted': NULL} AS quote, \
            map(['a', 'b'], [1, NULL]) AS labels, \
            array_value(1, NULL, 3) AS fixed_items";
    let decision = route_sql(sql).unwrap();
    let result =
        execute_typed_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap();

    let SqlTypedResult::Query { columns, rows } = result else {
        panic!("expected typed query result");
    };
    assert_eq!(
        columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type))
            .collect::<Vec<_>>(),
        vec![
            ("items", SqlType::Json),
            ("quote", SqlType::Json),
            ("labels", SqlType::Json),
            ("fixed_items", SqlType::Json),
        ]
    );
    assert_eq!(
        rows,
        vec![vec![
            SqlValue::Json(serde_json::json!([1, null, 2])),
            SqlValue::Json(serde_json::json!({"code": "AAPL", "price": "1.5", "halted": null})),
            SqlValue::Json(serde_json::json!([
                {"key": "a", "value": 1},
                {"key": "b", "value": null}
            ])),
            SqlValue::Json(serde_json::json!([1, null, 3])),
        ]]
    );
}

#[test]
fn shallow_complex_columns_query_and_restore_as_json() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE TABLE sector_snapshot(
            sector_code VARCHAR,
            stock_codes VARCHAR[],
            metrics STRUCT(total_count INTEGER, active_count INTEGER),
            extra MAP(VARCHAR, VARCHAR),
            ingest_at TIMESTAMP
        )",
    )
    .unwrap();

    let insert_sql = "\
        INSERT INTO sector_snapshot VALUES (
            'GN_SEMI',
            ['688981.SH', '002371.SZ'],
            {'total_count': 2, 'active_count': 2},
            map(['source', 'category'], ['xtquant', 'concept']),
            TIMESTAMP '2026-07-10 15:04:06'
        )";
    let insert_decision = route_sql(insert_sql).unwrap();
    execute_sql_blocking(
        &conn,
        "admin",
        insert_sql,
        insert_decision.route,
        &insert_decision.command,
        100,
    )
    .unwrap();

    assert_sector_snapshot_complex_json(&conn);

    let dir = std::env::temp_dir().join(format!(
        "rsduck_snapshot_complex_{}_{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
    let restored = Connection::open_in_memory().unwrap();
    restore_or_initialize(&restored, Some(&snapshot), "").unwrap();
    assert_sector_snapshot_complex_json(&restored);

    let _ = std::fs::remove_dir_all(dir);
}

fn assert_sector_snapshot_complex_json(conn: &Connection) {
    let query_sql = "\
        SELECT stock_codes, metrics, extra
        FROM sector_snapshot
        WHERE sector_code = 'GN_SEMI'";
    let query_decision = route_sql(query_sql).unwrap();
    let result = execute_typed_sql_blocking(
        conn,
        "admin",
        query_sql,
        query_decision.route,
        &query_decision.command,
        100,
    )
    .unwrap();
    let SqlTypedResult::Query { columns, rows } = result else {
        panic!("expected typed query result");
    };
    assert_eq!(
        columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type))
            .collect::<Vec<_>>(),
        vec![
            ("stock_codes", SqlType::Json),
            ("metrics", SqlType::Json),
            ("extra", SqlType::Json),
        ]
    );
    assert_eq!(
        rows,
        vec![vec![
            SqlValue::Json(serde_json::json!(["688981.SH", "002371.SZ"])),
            SqlValue::Json(serde_json::json!({"total_count": 2, "active_count": 2})),
            SqlValue::Json(serde_json::json!([
                {"key": "source", "value": "xtquant"},
                {"key": "category", "value": "concept"}
            ])),
        ]]
    );
}

#[test]
fn show_partitions_reports_partition_table_status() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE TABLE kline_1m(
            stock_code VARCHAR NOT NULL,
            trade_time TIMESTAMP NOT NULL,
            close DOUBLE,
            PRIMARY KEY(stock_code, trade_time)
        )
        PARTITION BY RANGE (trade_time)
        WITH (partition_unit = 'day', retention = '30')",
    )
    .unwrap();

    let insert_sql = "\
        INSERT INTO kline_1m(stock_code, trade_time, close)
        VALUES ('688981.SH', TIMESTAMP '2026-07-10 09:31:00', 50.2)";
    let insert_decision = route_sql(insert_sql).unwrap();
    execute_sql_blocking(
        &conn,
        "admin",
        insert_sql,
        insert_decision.route,
        &insert_decision.command,
        100,
    )
    .unwrap();

    let show_sql = "SHOW PARTITIONS FROM kline_1m";
    let show_decision = route_sql(show_sql).unwrap();
    let result = execute_typed_sql_blocking(
        &conn,
        "admin",
        show_sql,
        show_decision.route,
        &show_decision.command,
        100,
    )
    .unwrap();
    let SqlTypedResult::Query { columns, rows } = result else {
        panic!("expected SHOW PARTITIONS query result");
    };
    assert_eq!(
        columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "schema_name",
            "table_name",
            "partition_value",
            "physical_schema",
            "physical_table",
            "status",
            "row_count",
            "is_null_partition",
            "created_at",
            "activated_at",
            "dropped_at",
            "error_message",
        ]
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], SqlValue::Text("main".to_string()));
    assert_eq!(rows[0][1], SqlValue::Text("kline_1m".to_string()));
    assert_eq!(rows[0][2], SqlValue::Text("20260710".to_string()));
    assert_eq!(rows[0][3], SqlValue::Text("rsduck_internal".to_string()));
    assert_eq!(rows[0][5], SqlValue::Text("active".to_string()));
    assert_eq!(rows[0][6], SqlValue::Int64(1));
}

#[test]
fn bind_sql_params_rewrites_numbered_params_outside_literals() {
    let sql = "SELECT $1 AS a, '$2' AS literal, \"$3\" AS ident, -- $4\n$2 AS b, /* $5 */ $1 AS c";
    let bound = super::bind_sql_params(
        sql,
        &[
            SqlParam::Text("O'Reilly".to_string()),
            SqlParam::Integer(42),
        ],
    )
    .unwrap();

    assert_eq!(
        bound,
        "SELECT 'O''Reilly' AS a, '$2' AS literal, \"$3\" AS ident, -- $4\n42 AS b, /* $5 */ 'O''Reilly' AS c"
    );
    assert_eq!(super::sql_placeholder_count(sql).unwrap(), 2);
}

#[test]
fn internal_catalog_query_requires_catalog_diagnostic_privilege() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER operator_user PASSWORD='pw'")
        .unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER plain_user PASSWORD='pw'")
        .unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "GRANT ROLE operator TO operator_user")
        .unwrap();

    let sql = "SELECT * FROM rsduck_catalog.rs_relation";
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
fn reserved_catalog_queries_are_rejected_through_db_path() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();

    let sql = "INSERT INTO pg_catalog.blocked_relation VALUES (1)";
    let decision = route_sql(sql).unwrap();
    let err = execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
        .unwrap_err();
    assert_eq!(err, "reserved schema is managed by rsduck: pg_catalog");
}

#[test]
fn insert_with_chinese_text_executes_through_web_sql_path() {
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    let create_sql = "CREATE TABLE sector_list (
        sector_code VARCHAR,
        sector_name VARCHAR,
        category VARCHAR,
        constituent_count INTEGER,
        source VARCHAR,
        ingest_batch_id VARCHAR,
        ingest_at TIMESTAMP
    )";
    let decision = route_sql(create_sql).unwrap();
    execute_sql_blocking(
        &conn,
        "admin",
        create_sql,
        decision.route,
        &decision.command,
        100,
    )
    .unwrap();

    let insert_sql = "INSERT INTO sector_list
    VALUES
      ('GN_SEMI', '半导体', 'concept', 3, 'xtquant', 'batch_20260710_001', now()),
      ('SW_ELEC', '电子', 'sw_industry', 2, 'xtquant', 'batch_20260710_001', now())";
    let decision = route_sql(insert_sql).unwrap();
    let result = execute_sql_blocking(
        &conn,
        "admin",
        insert_sql,
        decision.route,
        &decision.command,
        100,
    )
    .unwrap();
    let SqlResult::Execute { affected_rows, .. } = result else {
        panic!("expected execute result");
    };
    assert_eq!(affected_rows, 2);

    let name: String = conn
        .query_row(
            "SELECT sector_name FROM sector_list WHERE sector_code = 'GN_SEMI'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "半导体");
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
    let snapshot_path = PathBuf::from(&snapshot);
    let manifest_path = snapshot_path.join(SNAPSHOT_MANIFEST_FILE);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["snapshot_format_version"], 2);
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
    assert!(snapshot_path.join("catalog.duckdb").is_file());
    let table_files = manifest["tables"].as_array().unwrap();
    assert_eq!(table_files.len(), 2);
    for table in table_files {
        assert!(snapshot_path
            .join(table["file"].as_str().unwrap())
            .is_file());
    }

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
fn snapshot_round_trip_restores_views_and_macros_from_duckdb_metadata() {
    let dir = std::env::temp_dir().join(format!(
        "rsduck_snapshot_metadata_round_trip_{}_{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE TABLE quotes(code VARCHAR, price DOUBLE)",
    )
    .unwrap();
    conn.execute_batch("INSERT INTO quotes VALUES ('AAPL', 10.0);")
        .unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE VIEW quote_codes AS SELECT code FROM quotes",
    )
    .unwrap();
    conn.execute_batch(
        "CREATE MACRO add_one(value) AS value + 1;
         CREATE MACRO quote_prices() AS TABLE SELECT code, price FROM quotes;",
    )
    .unwrap();

    let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(PathBuf::from(&snapshot).join(SNAPSHOT_MANIFEST_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["views"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["macros"].as_array().unwrap().len(), 2);
    assert!(manifest["views"][0]["ddl"]
        .as_str()
        .unwrap()
        .starts_with("CREATE VIEW"));

    let restored = Connection::open_in_memory().unwrap();
    restore_or_initialize(&restored, Some(&snapshot), "").unwrap();
    let view_value: String = restored
        .query_row("SELECT code FROM quote_codes", [], |row| row.get(0))
        .unwrap();
    let scalar_macro_value: i64 = restored
        .query_row("SELECT add_one(4)", [], |row| row.get(0))
        .unwrap();
    let table_macro_value: String = restored
        .query_row("SELECT code FROM quote_prices()", [], |row| row.get(0))
        .unwrap();
    assert_eq!(view_value, "AAPL");
    assert_eq!(scalar_macro_value, 5);
    assert_eq!(table_macro_value, "AAPL");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn snapshot_restore_rejects_tampered_view_ddl() {
    let dir = std::env::temp_dir().join(format!(
        "rsduck_snapshot_view_checksum_{}_{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
        .unwrap();
    crate::catalog::execute_catalog_aware_write(
        &conn,
        "CREATE VIEW quote_codes AS SELECT code FROM quotes",
    )
    .unwrap();

    let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
    let manifest_path = PathBuf::from(&snapshot).join(SNAPSHOT_MANIFEST_FILE);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["views"][0]["ddl"] =
        serde_json::Value::String("CREATE VIEW broken AS SELECT".to_string());
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let restored = Connection::open_in_memory().unwrap();
    let err = restore_or_initialize(&restored, Some(&snapshot), "").unwrap_err();
    assert_eq!(
        err,
        "snapshot view DDL checksum mismatch for main.quote_codes"
    );

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
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE table_a(id INTEGER)").unwrap();

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
    assert!(err.contains("snapshot manifest catalog metadata does not match catalog.duckdb"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn snapshot_restore_marks_missing_business_data_unavailable() {
    let dir = std::env::temp_dir().join(format!(
        "rsduck_snapshot_missing_data_{}_{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));
    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
        .unwrap();
    conn.execute("INSERT INTO quotes VALUES ('A')", []).unwrap();

    let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
    let manifest_path = PathBuf::from(&snapshot).join(SNAPSHOT_MANIFEST_FILE);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let data_file = manifest["tables"].as_array().unwrap().first().unwrap()["file"]
        .as_str()
        .unwrap();
    std::fs::remove_file(PathBuf::from(&snapshot).join(data_file)).unwrap();

    let restored = Connection::open_in_memory().unwrap();
    restore_or_initialize(&restored, Some(&snapshot), "").unwrap();
    let status: String = restored
        .query_row(
            "SELECT status FROM rsduck_catalog.rs_relation WHERE relname = 'quotes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "unavailable");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reset_admin_password_offline_exports_new_snapshot() {
    let dir = std::env::temp_dir().join(format!(
        "rsduck_reset_admin_password_{}_{}",
        std::process::id(),
        chrono::Local::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
    ));

    let conn = Connection::open_in_memory().unwrap();
    crate::catalog::bootstrap_fresh(&conn).unwrap();
    crate::catalog::execute_catalog_aware_write(&conn, "ALTER USER admin PASSWORD 'secret'")
        .unwrap();
    assert!(crate::catalog::authenticate_user(&conn, "admin", "secret").is_ok());
    assert!(crate::catalog::authenticate_user(&conn, "admin", "admin").is_err());

    let original = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let reset = reset_admin_password_offline(dir.to_str().unwrap(), "rsduck", "admin123").unwrap();

    assert_ne!(PathBuf::from(&original), PathBuf::from(&reset));
    assert!(PathBuf::from(&original).exists());
    assert!(PathBuf::from(&reset).exists());

    let restored = Connection::open_in_memory().unwrap();
    restore_or_initialize(&restored, Some(&reset), "").unwrap();
    assert!(crate::catalog::authenticate_user(&restored, "admin", "admin123").is_ok());
    assert!(crate::catalog::authenticate_user(&restored, "admin", "secret").is_err());

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

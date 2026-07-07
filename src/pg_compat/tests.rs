mod tests {
    use super::{compat_result, rewrite_sql};
    use crate::db::SqlResult;
    use crate::sql_route::{route_sql, SqlRoute};
    use duckdb::Connection;

    #[test]
    fn pg_database_relation_rewrite_preserves_filter_and_projection() {
        let conn = Connection::open_in_memory().unwrap();

        let sql = rewrite_sql("SELECT DISTINCT datlastsysoid FROM pg_database;")
            .expect("rewrite pg_database datlastsysoid");
        let datlastsysoid: String = conn
            .query_row(&sql, [], |row| row.get("datlastsysoid"))
            .unwrap();
        assert_eq!(datlastsysoid, "0");

        let missing_sql =
            rewrite_sql("SELECT datname FROM pg_catalog.pg_database WHERE datname = 'missing'")
                .expect("rewrite pg_database filter");
        let missing_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({missing_sql})"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing_count, 0);
    }

    #[test]
    fn regular_sql_is_not_intercepted() {
        assert!(compat_result("SELECT * FROM kline_day", "admin").is_none());
    }

    #[test]
    fn defined_empty_pg_catalog_relations_rewrite_to_empty_results() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        for relation in [
            "pg_trigger",
            "pg_extension",
            "pg_policy",
            "pg_matviews",
            "pg_sequences",
        ] {
            let sql = rewrite_sql(&format!("SELECT * FROM pg_catalog.{relation}"))
                .expect("rewrite empty pg_catalog relation");
            assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM ({sql})"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{relation}");
        }
    }

    #[test]
    fn pg_proc_rewrite_lists_builtin_compat_functions() {
        let conn = Connection::open_in_memory().unwrap();

        let sql = rewrite_sql(
            "SELECT proname, prorettype FROM pg_catalog.pg_proc WHERE proname IN ('format_type', 'has_table_privilege') ORDER BY proname",
        )
        .expect("rewrite pg_proc");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);
        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("proname")?,
                    row.get::<_, String>("prorettype")?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("format_type".to_string(), "25".to_string()),
                ("has_table_privilege".to_string(), "16".to_string()),
            ]
        );
    }

    #[test]
    fn navicat_datestyle_probe_returns_pg_compat_row() {
        let result = compat_result("SHOW DateStyle;", "admin").expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["DateStyle"]);
                assert_eq!(rows, vec![vec!["ISO, MDY"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn current_database_matches_duckdb_memory_catalog() {
        let result = compat_result("SELECT current_database();", "admin").expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_database"]);
                assert_eq!(rows, vec![vec!["memory"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn navicat_database_list_probe_returns_pg_compat_row() {
        let result = compat_result(
            "
            SELECT oid, datname AS databasename, pg_get_userbyid(datdba) AS databaseowner,
                   des.description AS description
            FROM pg_database d
            LEFT JOIN pg_shdescription des ON des.objoid = d.oid
            ",
            "admin",
        )
        .expect("compat result");

        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(&columns[0..3], ["oid", "databasename", "databaseowner"]);
                assert_eq!(rows[0][1], "memory");
                assert_eq!(rows[0][2], "admin");
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn rewrite_show_partitions_returns_partition_rows() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log (
                id BIGINT,
                access_time TIMESTAMP NOT NULL,
                content VARCHAR
            )
            PARTITION BY RANGE (access_time)
            WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time, content)
             VALUES (1, TIMESTAMP '2026-07-01 10:00:00', 'ok')",
        )
        .unwrap();

        let sql = rewrite_sql("SHOW PARTITIONS ods_access_log;").expect("rewrite show partitions");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);
        let partitions = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>("partition"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(partitions, vec!["20260701".to_string()]);

        let schema_sql = rewrite_sql("SHOW PARTITIONS main.ods_access_log")
            .expect("schema qualified show partitions");
        let schema_partitions = conn
            .prepare(&schema_sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>("partition"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(schema_partitions, vec!["20260701".to_string()]);
    }

    #[test]
    fn pg_session_setting_probes_are_accepted() {
        let show_result = compat_result("SHOW server_version;", "admin").expect("compat result");
        match show_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["server_version"]);
                assert_eq!(rows, vec![vec!["14.0"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let setting_result =
            compat_result("SELECT current_setting('server_version_num');", "admin")
                .expect("compat result");
        match setting_result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_setting"]);
                assert_eq!(rows, vec![vec!["140000"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }

        let set_result =
            compat_result("SET extra_float_digits = 3;", "admin").expect("compat result");
        match set_result {
            SqlResult::Execute {
                command,
                affected_rows,
            } => {
                assert_eq!(command, "SET");
                assert_eq!(affected_rows, 0);
            }
            SqlResult::Query { .. } => panic!("expected execute result"),
        }
    }

    #[test]
    fn pg_settings_relation_rewrite_preserves_filter_and_projection() {
        let conn = Connection::open_in_memory().unwrap();
        let sql = rewrite_sql(
            "SELECT name, setting FROM pg_catalog.pg_settings WHERE name = 'server_version_num'",
        )
        .expect("rewrite pg_settings sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("server_version_num".to_string(), "140000".to_string())]
        );
    }

    #[test]
    fn current_user_uses_authenticated_session_user() {
        let result = compat_result("SELECT current_user;", "alice").expect("compat result");
        match result {
            SqlResult::Query { columns, rows } => {
                assert_eq!(columns, vec!["current_user"]);
                assert_eq!(rows, vec![vec!["alice"]]);
            }
            SqlResult::Execute { .. } => panic!("expected query result"),
        }
    }

    #[test]
    fn pg_user_and_roles_rewrite_from_catalog_accounts() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let user_sql = rewrite_sql("SELECT usename FROM pg_catalog.pg_user").expect("pg_user");
        assert_eq!(route_sql(&user_sql).unwrap().route, SqlRoute::Read);
        let alice_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({user_sql}) WHERE usename = 'alice'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alice_count, 1);

        let roles_sql = rewrite_sql("SELECT rolname FROM pg_catalog.pg_roles").expect("pg_roles");
        assert_eq!(route_sql(&roles_sql).unwrap().route, SqlRoute::Read);
        let role_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({roles_sql}) WHERE rolname IN ('admin', 'reader')"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role_count, 2);
    }

    #[test]
    fn owner_projection_uses_catalog_user_names() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER owner_user PASSWORD='pw'")
            .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE SCHEMA owned_schema",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON SCHEMA owned_schema IS 'owned schema comment'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE TABLE owned_schema.owned_table(id INTEGER)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write_as(
            &conn,
            "owner_user",
            "CREATE VIEW owned_schema.owned_view AS SELECT * FROM owned_schema.owned_table",
        )
        .unwrap();

        let schema_sql = rewrite_sql(
            "SELECT schema_owner, description FROM information_schema.schemata WHERE schema_name = 'owned_schema'",
        )
        .expect("rewrite schema owner");
        let (schema_owner, schema_description): (String, String) = conn
            .query_row(&schema_sql, [], |row| {
                Ok((row.get("schema_owner")?, row.get("description")?))
            })
            .unwrap();
        assert_eq!(schema_owner, "owner_user");
        assert_eq!(schema_description, "owned schema comment");

        let standard_schema_sql = rewrite_sql(
            "SELECT catalog_name, default_character_set_name FROM information_schema.schemata WHERE schema_name = 'owned_schema'",
        )
        .expect("rewrite standard schema columns");
        let (catalog_name, charset_name): (String, String) = conn
            .query_row(&standard_schema_sql, [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(catalog_name, "memory");
        assert_eq!(charset_name, "UTF8");

        let table_sql = rewrite_sql(
            "SELECT tableowner FROM pg_catalog.pg_tables WHERE tablename = 'owned_table'",
        )
        .expect("rewrite table owner");
        let table_owner: String = conn
            .query_row(&table_sql, [], |row| row.get("tableowner"))
            .unwrap();
        assert_eq!(table_owner, "owner_user");

        let view_sql =
            rewrite_sql("SELECT viewowner FROM pg_catalog.pg_views WHERE viewname = 'owned_view'")
                .expect("rewrite view owner");
        let view_owner: String = conn
            .query_row(&view_sql, [], |row| row.get("viewowner"))
            .unwrap();
        assert_eq!(view_owner, "owner_user");
    }

    #[test]
    fn pg_class_rewrite_returns_duckdb_tables() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR NOT NULL, bar_time TIMESTAMP NOT NULL, close DOUBLE, PRIMARY KEY(code, bar_time))",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT c.oid, c.relname, n.nspname
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE c.relkind IN ('r', 'p')
            ",
        )
        .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let table_name: String = conn.query_row(&sql, [], |row| row.get("relname")).unwrap();
        assert_eq!(table_name, "kline_day");
    }

    #[test]
    fn pg_class_rewrite_returns_partitioned_table_relkind() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();

        let sql =
            rewrite_sql("SELECT relname, relkind FROM pg_catalog.pg_class").expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let relkind: String = conn
            .query_row(
                &format!("SELECT relkind FROM ({sql}) WHERE relname = 'ods_access_log'"),
                [],
                |row| row.get("relkind"),
            )
            .unwrap();
        assert_eq!(relkind, "p");
    }

    #[test]
    fn navicat_table_list_query_rewrites_supported_pg_catalog_relations() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time)
             VALUES (1, TIMESTAMP '2026-07-01 10:00:00')",
        )
        .unwrap();

        let sql = rewrite_sql(
            r#"
            SELECT c.oid,
                   n.nspname AS schemaname,
                   c.relname AS tablename,
                   c.relacl,
                   pg_get_userbyid(c.relowner) AS tableowner,
                   obj_description(c.oid) AS description,
                   c.relkind,
                   ci.relname AS cluster,
                   c.relhasindex AS hasindexes,
                   c.relhasrules AS hasrules,
                   t.spcname AS tablespace,
                   c.reloptions AS param,
                   c.relhastriggers AS hastriggers,
                   c.relpersistence AS unlogged,
                   ft.ftoptions,
                   fs.srvname,
                   c.relispartition,
                   pg_get_expr(c.relpartbound, c.oid) AS relpartbound,
                   c.reltuples,
                   ((SELECT count(*) FROM pg_inherits WHERE inhparent = c.oid) > 0) AS inhtable,
                   i2.nspname AS inhschemaname,
                   i2.relname AS inhtablename
            FROM pg_class c
            LEFT JOIN pg_namespace n ON n.oid = c.relnamespace
            LEFT JOIN pg_tablespace t ON t.oid = c.reltablespace
            LEFT JOIN (
                pg_inherits i
                INNER JOIN pg_class c2 ON i.inhparent = c2.oid
                LEFT JOIN pg_namespace n2 ON n2.oid = c2.relnamespace
            ) i2 ON i2.inhrelid = c.oid
            LEFT JOIN pg_index ind ON (ind.indrelid = c.oid) AND (ind.indisclustered = 't')
            LEFT JOIN pg_class ci ON ci.oid = ind.indexrelid
            LEFT JOIN pg_foreign_table ft ON ft.ftrelid = c.oid
            LEFT JOIN pg_foreign_server fs ON ft.ftserver = fs.oid
            WHERE ((c.relkind = 'r'::"char") OR (c.relkind = 'f'::"char") OR (c.relkind = 'p'::"char"))
              AND n.nspname = 'main'
            ORDER BY schemaname, tablename
            "#,
        )
        .expect("rewrite navicat table list query");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("tablename")?,
                    row.get::<_, String>("relkind")?,
                    row.get::<_, bool>("inhtable")?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(rows.contains(&("kline_day".to_string(), "r".to_string(), false)));
        assert!(rows.contains(&("ods_access_log".to_string(), "p".to_string(), true)));
    }

    #[test]
    fn navicat_partitioned_table_column_query_rewrites_any_conkey() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(
                id BIGINT,
                user_id VARCHAR,
                access_time TIMESTAMP NOT NULL,
                content TEXT
             )
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();

        let sql = rewrite_sql(
            r#"
            SELECT attname AS name,
                   attrelid AS tid,
                   coalesce((
                       SELECT attnum = ANY (conkey)
                       FROM pg_constraint
                       WHERE contype = 'p' AND conrelid = attrelid
                   ), false) AS primarykey,
                   not(attnotnull) AS allownull,
                   EXISTS(
                       SELECT seq.oid
                       FROM pg_class seq
                       LEFT JOIN pg_depend dep ON seq.oid = dep.objid
                       WHERE seq.relkind = 'S'::char
                         AND dep.refobjsubid = attnum
                         AND dep.refobjid = attrelid
                   ) AS autoincrement
            FROM pg_attribute
            WHERE attisdropped = false
              AND attrelid = (
                  SELECT tbl.oid
                  FROM pg_class tbl
                  LEFT JOIN pg_namespace sch ON tbl.relnamespace = sch.oid
                  WHERE tbl.relkind = 'p'::"char"
                    AND tbl.relname = 'ods_access_log'
                    AND sch.nspname = 'main'
              )
              AND (attname = 'id' OR attname = 'user_id' OR attname = 'access_time' OR attname = 'content')
            "#,
        )
        .expect("rewrite navicat partitioned table column query");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("name")?,
                    row.get::<_, bool>("primarykey")?,
                    row.get::<_, bool>("allownull")?,
                    row.get::<_, bool>("autoincrement")?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|(_, primarykey, _, autoincrement)| {
            !primarykey && !autoincrement
        }));
    }

    #[test]
    fn navicat_column_detail_query_rewrites_collation_sequence_and_pg_casts() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(
                code VARCHAR NOT NULL,
                bar_time TIMESTAMP NOT NULL,
                close DOUBLE,
                PRIMARY KEY(code, bar_time)
             )",
        )
        .unwrap();

        let sql = rewrite_sql(
            r#"
            SELECT col.table_schema AS schema_name,
                   col.table_name,
                   col.column_name,
                   col.character_maximum_length,
                   col.is_nullable,
                   col.numeric_precision,
                   col.numeric_scale,
                   col.datetime_precision,
                   col.ordinal_position,
                   b.atttypmod,
                   b.attndims,
                   col.data_type AS col_type,
                   et.typelem,
                   et.typlen,
                   et.typtype,
                   nbt.nspname AS elem_schema,
                   bt.typname AS elem_name,
                   b.atttypid,
                   col.udt_schema,
                   col.udt_name,
                   col.domain_catalog,
                   col.domain_schema,
                   col.domain_name,
                   col_description(c.oid, col.ordinal_position) AS comment,
                   col.column_default AS col_default,
                   col.is_identity,
                   col.identity_generation,
                   col.identity_start,
                   col.identity_increment,
                   col.identity_maximum,
                   col.identity_minimum,
                   seq.seqcache::information_schema.character_data AS identity_cache,
                   col.identity_cycle,
                   col.is_generated,
                   col.generation_expression,
                   b.attacl,
                   colnsp.nspname AS collation_schema_name,
                   coll.collname,
                   c.relkind,
                   b.attfdwoptions AS foreign_options
            FROM information_schema.columns AS col
            LEFT JOIN pg_namespace ns ON ns.nspname = col.table_schema
            LEFT JOIN pg_class c ON col.table_name = c.relname AND c.relnamespace = ns.oid
            LEFT JOIN pg_attrdef a ON c.oid = a.adrelid AND col.ordinal_position = a.adnum
            LEFT JOIN pg_attribute b ON b.attrelid = c.oid AND b.attname = col.column_name
            LEFT JOIN pg_type et ON et.oid = b.atttypid
            LEFT JOIN pg_collation coll ON coll.oid = b.attcollation
            LEFT JOIN pg_namespace colnsp ON coll.collnamespace = colnsp.oid
            LEFT JOIN (
                pg_depend dep
                JOIN pg_sequence seq
                  ON dep.classid = 'pg_class'::regclass::oid
                 AND dep.objid = seq.seqrelid
                 AND dep.deptype = 'i'::"char"
            )
              ON dep.refclassid = 'pg_class'::regclass::oid
             AND dep.refobjid = c.oid
             AND dep.refobjsubid = b.attnum
            LEFT JOIN pg_type bt ON et.typelem = bt.oid
            LEFT JOIN pg_namespace nbt ON bt.typnamespace = nbt.oid
            WHERE col.table_schema = 'main'
              AND col.table_name = 'kline_day'
            ORDER BY col.table_schema, col.table_name, col.ordinal_position
            "#,
        )
        .expect("rewrite navicat column detail query");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("column_name")?,
                    row.get::<_, String>("col_type")?,
                    row.get::<_, String>("relkind")?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            (
                "code".to_string(),
                "character varying".to_string(),
                "r".to_string()
            )
        );
    }

    #[test]
    fn pg_attribute_rewrite_returns_duckdb_columns() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT a.attname
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            WHERE c.relname = 'kline_day'
            ORDER BY CAST(a.attnum AS INTEGER)
            ",
        )
        .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let column_name: String = conn.query_row(&sql, [], |row| row.get("attname")).unwrap();
        assert_eq!(column_name, "code");
    }

    #[test]
    fn catalog_relation_rewrite_handles_pg_function_expressions() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(
                code VARCHAR NOT NULL,
                bar_time TIMESTAMP NOT NULL,
                close DOUBLE,
                PRIMARY KEY(code, bar_time)
            )",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT
                c.relname,
                pg_catalog.pg_get_userbyid(c.relowner) AS owner_name,
                pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
            WHERE c.relname = 'kline_day' AND a.attname = 'code'
            ",
        )
        .expect("rewrite catalog function expression sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let (owner_name, data_type): (String, String) = conn
            .query_row(&sql, [], |row| {
                Ok((row.get("owner_name")?, row.get("data_type")?))
            })
            .unwrap();
        assert_eq!(owner_name, "admin");
        assert_eq!(data_type, "varchar");

        let constraint_sql = rewrite_sql(
            "
            SELECT con.conname, pg_get_constraintdef(con.oid) AS constraintdef
            FROM pg_catalog.pg_constraint con
            WHERE con.conname = 'kline_day_pkey'
            ",
        )
        .expect("rewrite catalog constraint function expression sql");
        let constraintdef: String = conn
            .query_row(&constraint_sql, [], |row| row.get("constraintdef"))
            .unwrap();
        assert_eq!(constraintdef, "PRIMARY KEY (code, bar_time)");
    }

    #[test]
    fn information_schema_columns_rewrite_returns_standard_column_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR NOT NULL, close DOUBLE DEFAULT 0, bar_time TIMESTAMP)",
        )
        .unwrap();

        let sql = rewrite_sql(
            "
            SELECT table_catalog, table_schema, table_name, column_name, ordinal_position,
                   data_type, is_nullable, column_default, udt_name
            FROM information_schema.columns
            WHERE table_name = 'kline_day'
            ORDER BY CAST(ordinal_position AS INTEGER)
            ",
        )
        .expect("rewrite information_schema.columns sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let rows = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>("table_catalog")?,
                    row.get::<_, String>("table_schema")?,
                    row.get::<_, String>("table_name")?,
                    row.get::<_, String>("column_name")?,
                    row.get::<_, String>("ordinal_position")?,
                    row.get::<_, String>("data_type")?,
                    row.get::<_, String>("is_nullable")?,
                    row.get::<_, String>("column_default")?,
                    row.get::<_, String>("udt_name")?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rows[0].0, "memory");
        assert_eq!(rows[0].1, "main");
        assert_eq!(rows[0].2, "kline_day");
        assert_eq!(rows[0].3, "code");
        assert_eq!(rows[0].4, "1");
        assert_eq!(rows[0].5, "character varying");
        assert_eq!(rows[0].6, "NO");
        assert_eq!(rows[0].8, "varchar");

        assert_eq!(rows[1].3, "close");
        assert_eq!(rows[1].5, "double precision");
        assert_eq!(rows[1].6, "YES");
        assert_eq!(rows[1].7, "0");
        assert_eq!(rows[1].8, "float8");

        assert_eq!(rows[2].3, "bar_time");
        assert_eq!(rows[2].5, "timestamp without time zone");
        assert_eq!(rows[2].8, "timestamp");
    }

    #[test]
    fn pg_attribute_rewrite_only_returns_dropped_columns_when_requested() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "ALTER TABLE kline_day DROP COLUMN close",
        )
        .unwrap();

        let default_sql = rewrite_sql(
            "
            SELECT a.attname
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            WHERE c.relname = 'kline_day' AND a.attname = 'close'
            ",
        )
        .expect("rewrite default pg_attribute sql");
        let default_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({default_sql})"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(default_count, 0);

        let dropped_sql = rewrite_sql(
            "
            SELECT a.attname, a.attisdropped
            FROM pg_catalog.pg_attribute a
            JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
            WHERE c.relname = 'kline_day' AND a.attisdropped = 't'
            ",
        )
        .expect("rewrite dropped pg_attribute sql");
        let (attname, attisdropped): (String, String) = conn
            .query_row(&dropped_sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!(attname, "close");
        assert_eq!(attisdropped, "t");
    }

    #[test]
    fn pg_class_tables_and_information_schema_tables_have_distinct_shapes() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE VIEW kline_view AS SELECT code FROM kline_day",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE INDEX idx_kline_day_code ON kline_day(code)",
        )
        .unwrap();

        let class_sql = rewrite_sql(
            "SELECT relname, relkind FROM pg_catalog.pg_class WHERE relname = 'idx_kline_day_code'",
        )
        .expect("rewrite pg_class sql");
        let index_relkind: String = conn
            .query_row(&class_sql, [], |row| row.get("relkind"))
            .unwrap();
        assert_eq!(index_relkind, "i");

        let tables_sql = rewrite_sql("SELECT tablename FROM pg_catalog.pg_tables")
            .expect("rewrite pg_tables sql");
        let pg_tables_index_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM ({tables_sql}) WHERE tablename = 'idx_kline_day_code'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pg_tables_index_count, 0);

        let info_sql = rewrite_sql(
            "SELECT table_name, table_type FROM information_schema.tables ORDER BY table_name",
        )
        .expect("rewrite information_schema.tables sql");
        let info_index_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM ({info_sql}) WHERE table_name = 'idx_kline_day_code'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(info_index_count, 0);
        let navicat_style_info_sql = rewrite_sql(
            "SELECT table_catalog, table_schema, table_name
             FROM information_schema.tables
             WHERE table_catalog = current_database() AND table_schema = 'main'",
        )
        .expect("rewrite current database information_schema.tables sql");
        let table_count_for_current_database: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM ({navicat_style_info_sql})"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count_for_current_database, 2);
        let view_type: String = conn
            .query_row(
                &format!("SELECT table_type FROM ({info_sql}) WHERE table_name = 'kline_view'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_type, "VIEW");
    }

    #[test]
    fn pg_type_projection_hides_internal_partition_row_types() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(id BIGINT, access_time TIMESTAMP NOT NULL)
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time) VALUES
             (1, TIMESTAMP '2026-07-01 10:00:00')",
        )
        .unwrap();

        let type_sql = rewrite_sql(
            "SELECT typname FROM pg_catalog.pg_type WHERE typname LIKE 'ods_access_log%'",
        )
        .expect("rewrite pg_type sql");
        let type_names = conn
            .prepare(&type_sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>("typname"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(type_names, vec!["ods_access_log".to_string()]);
    }

    #[test]
    fn pg_attrdef_projection_hides_internal_partition_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE ods_access_log(
                id BIGINT,
                access_time TIMESTAMP NOT NULL,
                source TEXT DEFAULT 'web'
             )
             PARTITION BY RANGE (access_time)
             WITH (partition_unit = 'day', retention = '30')",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "INSERT INTO ods_access_log(id, access_time) VALUES
             (1, TIMESTAMP '2026-07-01 10:00:00')",
        )
        .unwrap();

        let attrdef_sql =
            rewrite_sql("SELECT adbin FROM pg_catalog.pg_attrdef").expect("rewrite pg_attrdef sql");
        let defaults = conn
            .prepare(&attrdef_sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>("adbin"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(defaults, vec!["'web'".to_string()]);
    }

    #[test]
    fn information_schema_views_uses_standard_view_columns() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE VIEW kline_view AS SELECT code FROM kline_day",
        )
        .unwrap();

        let info_views_sql = rewrite_sql(
            "SELECT table_schema, table_name, view_definition FROM information_schema.views",
        )
        .expect("rewrite information_schema.views sql");
        let (table_schema, table_name, view_definition): (String, String, String) = conn
            .query_row(&info_views_sql, [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();

        assert_eq!(table_schema, "main");
        assert_eq!(table_name, "kline_view");
        assert!(view_definition.contains("SELECT code FROM kline_day"));
    }

    #[test]
    fn pg_index_rewrite_returns_catalog_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(code VARCHAR, close DOUBLE)",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE INDEX idx_kline_day_code ON kline_day(code)",
        )
        .unwrap();

        let sql = rewrite_sql("SELECT indexrelid, indrelid FROM pg_catalog.pg_index")
            .expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let indexrelid: String = conn
            .query_row(&sql, [], |row| row.get("indexrelid"))
            .unwrap();
        assert!(!indexrelid.is_empty());

        let indexes_sql = rewrite_sql(
            "SELECT indexdef FROM pg_catalog.pg_indexes WHERE indexname = 'idx_kline_day_code'",
        )
        .expect("rewrite pg_indexes sql");
        let indexdef: String = conn
            .query_row(&indexes_sql, [], |row| row.get("indexdef"))
            .unwrap();
        assert_eq!(
            indexdef,
            "CREATE INDEX idx_kline_day_code ON main.kline_day (code)"
        );
    }

    #[test]
    fn information_schema_constraint_columns_rewrite_from_catalog_constraints() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE kline_day(
                code VARCHAR NOT NULL,
                bar_time TIMESTAMP NOT NULL,
                venue VARCHAR,
                close DOUBLE,
                PRIMARY KEY(code, bar_time),
                CONSTRAINT kline_day_close_key UNIQUE(close),
                CONSTRAINT kline_day_venue_key UNIQUE(venue)
            )",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE quote_tick(
                venue_ref VARCHAR,
                FOREIGN KEY(venue_ref) REFERENCES kline_day(venue)
            )",
        )
        .unwrap();

        let table_constraints_sql = rewrite_sql(
            "SELECT constraint_catalog, constraint_schema, constraint_name, table_catalog,
                    table_schema, table_name, constraint_type, is_deferrable,
                    initially_deferred, enforced
             FROM information_schema.table_constraints",
        )
        .expect("rewrite table constraints");
        assert_eq!(
            route_sql(&table_constraints_sql).unwrap().route,
            SqlRoute::Read
        );
        let (table_catalog, constraint_type, enforced): (String, String, String) = conn
            .query_row(
                &format!(
                    "SELECT table_catalog, constraint_type, enforced FROM ({table_constraints_sql}) \
                     WHERE constraint_name = 'kline_day_pkey'"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(table_catalog, "memory");
        assert_eq!(constraint_type, "PRIMARY KEY");
        assert_eq!(enforced, "YES");

        let key_sql = rewrite_sql(
            "SELECT constraint_name, column_name, ordinal_position, position_in_unique_constraint
             FROM information_schema.key_column_usage",
        )
        .expect("rewrite key column usage");
        assert_eq!(route_sql(&key_sql).unwrap().route, SqlRoute::Read);
        let first_key_column: String = conn
            .query_row(
                &format!(
                    "SELECT column_name FROM ({key_sql}) \
                     WHERE constraint_name = 'kline_day_pkey' \
                     ORDER BY CAST(ordinal_position AS INTEGER)"
                ),
                [],
                |row| row.get("column_name"),
            )
            .unwrap();
        assert_eq!(first_key_column, "code");

        let fk_position: String = conn
            .query_row(
                &format!(
                    "SELECT position_in_unique_constraint FROM ({key_sql}) \
                     WHERE constraint_name = 'quote_tick_fkey' AND column_name = 'venue_ref'"
                ),
                [],
                |row| row.get("position_in_unique_constraint"),
            )
            .unwrap();
        assert_eq!(fk_position, "1");

        let usage_sql = rewrite_sql(
            "SELECT table_name, column_name, constraint_name
             FROM information_schema.constraint_column_usage",
        )
        .expect("rewrite constraint column usage");
        assert_eq!(route_sql(&usage_sql).unwrap().route, SqlRoute::Read);
        let close_usage_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM ({usage_sql}) \
                     WHERE table_name = 'kline_day' AND column_name = 'close'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(close_usage_count, 1);

        let (fk_table_name, fk_column_name): (String, String) = conn
            .query_row(
                &format!(
                    "SELECT table_name, column_name FROM ({usage_sql}) \
                     WHERE constraint_name = 'quote_tick_fkey'"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(fk_table_name, "kline_day");
        assert_eq!(fk_column_name, "venue");
    }

    #[test]
    fn pg_catalog_scalar_functions_rewrite_to_catalog_queries() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE quotes(
                code VARCHAR,
                close DOUBLE DEFAULT 0,
                PRIMARY KEY(code)
            )",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON TABLE quotes IS 'quotes table'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "COMMENT ON COLUMN quotes.close IS 'close price'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE quote_ticks(
                code VARCHAR,
                CONSTRAINT quote_ticks_code_fkey FOREIGN KEY(code) REFERENCES quotes(code)
            )",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let type_sql = rewrite_sql("SELECT format_type(1043, -1)").expect("format_type rewrite");
        let type_name: String = conn
            .query_row(&type_sql, [], |row| row.get("format_type"))
            .unwrap();
        assert_eq!(type_name, "varchar");

        let expr_sql =
            rewrite_sql("SELECT pg_get_expr('close + 1', 0)").expect("pg_get_expr rewrite");
        let expr: String = conn
            .query_row(&expr_sql, [], |row| row.get("pg_get_expr"))
            .unwrap();
        assert_eq!(expr, "close + 1");

        let constraint_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_constraint WHERE conname = 'quotes_pkey'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let constraint_sql = rewrite_sql(&format!("SELECT pg_get_constraintdef({constraint_oid})"))
            .expect("pg_get_constraintdef rewrite");
        let constraint_def: String = conn
            .query_row(&constraint_sql, [], |row| row.get("pg_get_constraintdef"))
            .unwrap();
        assert_eq!(constraint_def, "PRIMARY KEY (code)");
        let fk_constraint_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_constraint WHERE conname = 'quote_ticks_code_fkey'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fk_constraint_sql =
            rewrite_sql(&format!("SELECT pg_get_constraintdef({fk_constraint_oid})"))
                .expect("foreign key constraintdef rewrite");
        let fk_constraint_def: String = conn
            .query_row(&fk_constraint_sql, [], |row| {
                row.get("pg_get_constraintdef")
            })
            .unwrap();
        assert_eq!(
            fk_constraint_def,
            "FOREIGN KEY (code) REFERENCES main.quotes (code)"
        );

        let table_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE SCHEMA archive").unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE archive.hidden_quotes(code VARCHAR)",
        )
        .unwrap();
        let hidden_table_oid: i64 = conn
            .query_row(
                "SELECT oid FROM rsduck_catalog.pg_class WHERE relname = 'hidden_quotes'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let visible_sql = rewrite_sql(&format!("SELECT pg_table_is_visible({table_oid})"))
            .expect("pg_table_is_visible main relation");
        let visible: String = conn
            .query_row(&visible_sql, [], |row| row.get("pg_table_is_visible"))
            .unwrap();
        assert_eq!(visible, "t");
        let hidden_visible_sql =
            rewrite_sql(&format!("SELECT pg_table_is_visible({hidden_table_oid})"))
                .expect("pg_table_is_visible non-main relation");
        let hidden_visible: String = conn
            .query_row(&hidden_visible_sql, [], |row| {
                row.get("pg_table_is_visible")
            })
            .unwrap();
        assert_eq!(hidden_visible, "f");

        let table_desc_sql =
            rewrite_sql(&format!("SELECT obj_description({table_oid})")).expect("description");
        let table_desc: String = conn
            .query_row(&table_desc_sql, [], |row| row.get("obj_description"))
            .unwrap();
        assert_eq!(table_desc, "quotes table");

        let close_attnum: i64 = conn
            .query_row(
                "SELECT a.attnum \
                 FROM rsduck_catalog.pg_attribute a \
                 JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
                 WHERE c.relname = 'quotes' AND a.attname = 'close'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let column_desc_sql = rewrite_sql(&format!(
            "SELECT col_description({table_oid}, {close_attnum})"
        ))
        .expect("column description");
        let column_desc: String = conn
            .query_row(&column_desc_sql, [], |row| row.get("col_description"))
            .unwrap();
        assert_eq!(column_desc, "close price");

        let alice_id: i64 = conn
            .query_row(
                "SELECT user_id FROM rsduck_catalog.rs_user WHERE username = 'alice'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let user_sql =
            rewrite_sql(&format!("SELECT pg_get_userbyid({alice_id})")).expect("user lookup");
        let username: String = conn
            .query_row(&user_sql, [], |row| row.get("pg_get_userbyid"))
            .unwrap();
        assert_eq!(username, "alice");
    }

    #[test]
    fn pg_namespace_rewrite_returns_duckdb_schemas() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        let sql =
            rewrite_sql("SELECT oid, nspname FROM pg_catalog.pg_namespace").expect("rewrite sql");
        assert_eq!(route_sql(&sql).unwrap().route, SqlRoute::Read);

        let schema_name: String = conn.query_row(&sql, [], |row| row.get("nspname")).unwrap();
        assert_eq!(schema_name, "main");
    }
}

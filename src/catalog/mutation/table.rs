fn create_table_relation(
    conn: &Connection,
    create_table: &CreateTable,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_table.partition_by.is_some() {
        return Err("managed range partitioned table mutation is not implemented yet".into());
    }
    if create_table.query.is_some() {
        return Err("CREATE TABLE AS is not supported by catalog mutation yet".into());
    }
    if create_table.temporary {
        return Err("temporary table is not supported by rsduck catalog".into());
    }

    let (schema, table) = relation_name(&create_table.name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &table)? {
            if create_table.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{table}"));
        }

        validate_create_table_column_types(create_table)?;
        ensure_user_schema_exists(conn, &schema)?;
        let rel_oid = allocate_oid(conn)?;
        let type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_table", rel_oid, sql)?;

        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB CREATE TABLE failed: {e}"))?;

        let columns = load_duckdb_columns(conn, &schema, &table)?;
        insert_relation_rows(
            conn,
            rel_oid,
            type_oid,
            &schema,
            &table,
            "r",
            "ordinary",
            "user",
            "",
            &columns,
            owner_user_id,
        )?;
        insert_create_table_constraints(conn, rel_oid, &schema, &table, &columns, create_table)?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn create_range_partitioned_table(
    conn: &Connection,
    partitioned: &ManagedPartitionCreate,
    owner_user_id: i64,
) -> Result<usize, String> {
    let (statement, _) = parse_one_statement(&partitioned.base_sql)?;
    let Statement::CreateTable(create_table) = statement else {
        return Err("managed partitioned table base DDL must be CREATE TABLE".into());
    };
    if create_table.query.is_some() {
        return Err("CREATE TABLE AS is not supported by managed partitioned table".into());
    }
    if create_table.temporary {
        return Err("temporary managed partitioned table is not supported".into());
    }
    let (schema, table) = relation_name(&create_table.name)?;
    reject_reserved_schema(&schema)?;
    let null_partition = physical_partition_name(&table, "_null");
    let view_sql =
        partition_entrypoint_sql(&schema, &table, &[("rsduck_internal", &null_partition)]);

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &table)? {
            if create_table.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{table}"));
        }
        if relation_exists(conn, "rsduck_internal", &null_partition)? {
            return Err(format!(
                "managed physical partition relation already exists: rsduck_internal.{null_partition}"
            ));
        }

        validate_create_table_column_types(&create_table)?;
        let (partition_key_type, _) = validate_partition_key(
            &create_table,
            &partitioned.partition_key,
            &partitioned.partition_unit,
        )?;
        ensure_user_schema_exists(conn, &schema)?;
        let parent_oid = allocate_oid(conn)?;
        let parent_type_oid = allocate_oid(conn)?;
        let child_oid = allocate_oid(conn)?;
        let child_type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(
            conn,
            "create_range_partitioned_table",
            parent_oid,
            &partitioned.base_sql,
        )?;

        let create_null_sql = physical_partition_create_sql(&null_partition, &create_table);
        conn.execute(&create_null_sql, [])
            .map_err(|e| format!("execute DuckDB CREATE null partition failed: {e}"))?;
        conn.execute(&view_sql, [])
            .map_err(|e| format!("execute DuckDB CREATE partition entrypoint failed: {e}"))?;

        let columns = load_duckdb_columns(conn, "rsduck_internal", &null_partition)?;
        insert_relation_rows(
            conn,
            parent_oid,
            parent_type_oid,
            &schema,
            &table,
            "p",
            "range_partitioned_table",
            "user",
            &view_sql,
            &columns,
            owner_user_id,
        )?;
        insert_create_table_constraints(
            conn,
            parent_oid,
            &schema,
            &table,
            &columns,
            &create_table,
        )?;
        update_partition_relation_ext(
            conn,
            parent_oid,
            &partitioned.partition_key,
            &partition_key_type,
            &partitioned.partition_unit,
            partitioned.retention_count,
            &view_sql,
        )?;

        insert_relation_rows(
            conn,
            child_oid,
            child_type_oid,
            "rsduck_internal",
            &null_partition,
            "r",
            "physical_partition",
            "internal",
            "",
            &columns,
            owner_user_id,
        )?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class \
                 SET relispartition = TRUE, relpartbound = '_null' \
                 WHERE oid = {child_oid}"
            ),
            [],
        )
        .map_err(|e| format!("mark null partition pg_class failed: {e}"))?;
        update_partition_relation_ext(
            conn,
            child_oid,
            &partitioned.partition_key,
            &partition_key_type,
            "null",
            0,
            "",
        )?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_partition(parent_relid, child_relid, partition_value, \
                 partition_unit, lower_bound, upper_bound, is_null_partition, status, row_count, min_ts, \
                 max_ts, checksum, created_at, activated_at, dropped_at, error_message) \
                 VALUES ({parent_oid}, {child_oid}, '_null', 'null', NULL, NULL, TRUE, 'active', 0, \
                 NULL, NULL, '', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, '')"
            ),
            [],
        )
        .map_err(|e| format!("write null partition metadata failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({PG_CLASS_CLASSOID}, {parent_oid}, 0, {PG_CLASS_CLASSOID}, {child_oid}, 0, 'n')"
            ),
            [],
        )
        .map_err(|e| format!("write partition dependency failed: {e}"))?;

        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn insert_partitioned_relation(
    conn: &Connection,
    insert: &Insert,
    sql: &str,
) -> Result<usize, String> {
    let TableObject::TableName(table_name) = &insert.table else {
        return Ok(0);
    };
    let (schema, table) = relation_name(table_name)?;
    reject_reserved_schema(&schema)?;
    let Some(relation) = partitioned_relation(conn, &schema, &table)? else {
        return Ok(0);
    };
    if insert.source.is_none() {
        return Err("INSERT into managed partitioned table requires a source query".into());
    }
    if !insert.assignments.is_empty()
        || insert.returning.is_some()
        || insert.on.is_some()
        || insert.overwrite
        || insert.partitioned.is_some()
        || insert.format_clause.is_some()
    {
        return Err("unsupported INSERT form for managed partitioned table".into());
    }

    let target_columns = insert_target_columns(insert, &relation)?;
    let partition_key_idx = target_columns
        .iter()
        .position(|column| column.eq_ignore_ascii_case(&relation.partition_key));
    let source = insert.source.as_ref().expect("source checked");

    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "insert_partitioned_rows", relation.oid, sql)?;
        let groups =
            partition_insert_groups(conn, source, &target_columns, partition_key_idx, &relation)?;

        let mut affected = 0usize;
        for (partition_value, route_ts, rows) in groups {
            let child_relname = ensure_active_partition(conn, &relation, &partition_value)?;
            let values_sql = rows
                .iter()
                .map(|row| format!("({})", row.join(", ")))
                .collect::<Vec<_>>()
                .join(", ");
            let insert_sql = format!(
                "INSERT INTO {} ({}) VALUES {values_sql}",
                quote_qualified("rsduck_internal", &child_relname),
                target_columns
                    .iter()
                    .map(|column| quote_ident(column))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            conn.execute(&insert_sql, [])
                .map_err(|e| format!("insert partition rows failed: {e}"))?;
            update_partition_stats(
                conn,
                relation.oid,
                &partition_value,
                rows.len() as i64,
                route_ts,
            )?;
            affected += rows.len();
        }

        expire_old_partitions(conn, &relation)?;
        refresh_partition_entrypoint(conn, relation.oid, &relation.schema, &relation.name)?;
        finish_journal(conn, journal_id)?;
        Ok(affected)
    })
}

type PartitionInsertGroups = Vec<(String, Option<NaiveDateTime>, Vec<Vec<String>>)>;


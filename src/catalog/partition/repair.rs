fn mark_partition_unavailable(
    conn: &Connection,
    schema: &str,
    table: &str,
    partition_value: &str,
    reason: &str,
    sql: &str,
) -> Result<usize, String> {
    let relation = partitioned_relation(conn, schema, table)?
        .ok_or_else(|| format!("partitioned relation does not exist: {schema}.{table}"))?;
    run_catalog_tx(conn, || {
        let partition =
            partition_child_by_value(conn, relation.oid, partition_value)?.ok_or_else(|| {
                format!("partition does not exist: {schema}.{table} {partition_value}")
            })?;
        let journal_id =
            insert_journal(conn, "mark_partition_unavailable", partition.child_oid, sql)?;
        let error_message = relation_unavailable_message(partition.child_oid, reason);
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_partition \
                 SET status = 'failed', error_message = '{}' \
                 WHERE parent_relid = {} AND child_relid = {}",
                sql_string(&error_message),
                relation.oid,
                partition.child_oid
            ),
            [],
        )
        .map_err(|e| format!("mark partition unavailable failed: {e}"))?;
        mark_relation_unavailable(conn, partition.child_oid, reason)?;
        if partition.is_null_partition {
            mark_relation_unavailable(conn, relation.oid, "null partition unavailable")?;
        } else {
            refresh_partition_entrypoint(conn, relation.oid, schema, table)?;
        }
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

fn cleanup_null_partition(
    conn: &Connection,
    schema: &str,
    table: &str,
    mode: &str,
    sql: &str,
) -> Result<usize, String> {
    if !mode.eq_ignore_ascii_case("clear") {
        return Err(format!(
            "unsupported null partition cleanup mode: {mode}; supported mode: clear"
        ));
    }
    let relation = partitioned_relation(conn, schema, table)?
        .ok_or_else(|| format!("partitioned relation does not exist: {schema}.{table}"))?;
    run_catalog_tx(conn, || {
        let partition = partition_child_by_value(conn, relation.oid, "_null")?
            .ok_or_else(|| format!("null partition does not exist: {schema}.{table}"))?;
        let journal_id = insert_journal(conn, "cleanup_null_partition", partition.child_oid, sql)?;
        conn.execute(
            &format!(
                "DELETE FROM {}",
                quote_qualified(&partition.schema, &partition.relname)
            ),
            [],
        )
        .map_err(|e| format!("cleanup null partition rows failed: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_partition \
                 SET row_count = 0, min_ts = NULL, max_ts = NULL, checksum = '', error_message = '' \
                 WHERE parent_relid = {} AND child_relid = {}",
                relation.oid, partition.child_oid
            ),
            [],
        )
        .map_err(|e| format!("update null partition cleanup metadata failed: {e}"))?;
        refresh_partition_entrypoint(conn, relation.oid, schema, table)?;
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

fn repair_partition(
    conn: &Connection,
    schema: &str,
    table: &str,
    partition_value: &str,
    sql: &str,
) -> Result<usize, String> {
    let relation = partitioned_relation(conn, schema, table)?
        .ok_or_else(|| format!("partitioned relation does not exist: {schema}.{table}"))?;
    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "repair_partition", relation.oid, sql)?;
        if partition_value == "_null" {
            let partition = partition_child_by_value(conn, relation.oid, "_null")?
                .ok_or_else(|| format!("null partition does not exist: {schema}.{table}"))?;
            validate_table_physical(
                conn,
                partition.child_oid,
                &partition.schema,
                &partition.relname,
            )?;
            conn.execute(
                &format!(
                    "UPDATE rsduck_catalog.rs_partition SET status = 'active', error_message = '' \
                     WHERE parent_relid = {} AND child_relid = {}",
                    relation.oid, partition.child_oid
                ),
                [],
            )
            .map_err(|e| format!("repair null partition metadata failed: {e}"))?;
            conn.execute(
                &format!(
                    "UPDATE rsduck_catalog.pg_class SET status = 'active', error_message = '' \
                     WHERE oid IN ({}, {})",
                    relation.oid, partition.child_oid
                ),
                [],
            )
            .map_err(|e| format!("repair null partition relation status failed: {e}"))?;
            refresh_partition_entrypoint(conn, relation.oid, schema, table)?;
            finish_journal(conn, journal_id)?;
            return Ok(1);
        }

        if let Some(partition) = active_partition_by_value(conn, relation.oid, partition_value)? {
            validate_table_physical(
                conn,
                partition.child_oid,
                &partition.schema,
                &partition.relname,
            )?;
            refresh_partition_entrypoint(conn, relation.oid, schema, table)?;
            finish_journal(conn, journal_id)?;
            return Ok(0);
        }

        repair_non_active_partition(conn, &relation, partition_value)?;
        refresh_partition_entrypoint(conn, relation.oid, schema, table)?;
        finish_journal(conn, journal_id)?;
        Ok(1)
    })
}

fn repair_non_active_partition(
    conn: &Connection,
    relation: &PartitionedRelation,
    partition_value: &str,
) -> Result<(), String> {
    let (child_oid, partition_status): (i64, String) = conn
        .query_row(
            &format!(
                "SELECT child_relid, status \
                 FROM rsduck_catalog.rs_partition \
                 WHERE parent_relid = {} AND partition_value = '{}'",
                relation.oid,
                sql_string(partition_value)
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| {
            format!(
                "partition metadata does not exist: {} partition_value={partition_value}: {e}",
                relation.name
            )
        })?;
    if partition_status == "active" {
        return Ok(());
    }

    let child_relname = physical_partition_name(&relation.name, partition_value);
    if !relation_exists(conn, "rsduck_internal", &child_relname)? {
        let child_type_oid = allocate_oid(conn)?;
        let create_sql = physical_partition_create_from_catalog_sql(
            conn,
            relation.oid,
            "rsduck_internal",
            &child_relname,
            &relation.columns,
        )?;
        conn.execute(&create_sql, [])
            .map_err(|e| format!("execute DuckDB CREATE repaired partition failed: {e}"))?;
        let columns = load_duckdb_columns(conn, "rsduck_internal", &child_relname)?;
        insert_relation_rows(
            conn,
            child_oid,
            child_type_oid,
            "rsduck_internal",
            &child_relname,
            "r",
            "physical_partition",
            "internal",
            "",
            &columns,
            ADMIN_USER_ID,
        )?;
        let bounds = partition_bounds(partition_value, &relation.partition_unit)?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.pg_class \
                 SET relispartition = TRUE, relpartbound = '{}' \
                 WHERE oid = {child_oid}",
                sql_string(&format!("[{}, {})", bounds.lower_bound, bounds.upper_bound))
            ),
            [],
        )
        .map_err(|e| format!("mark repaired partition pg_class failed: {e}"))?;
        update_partition_relation_ext(
            conn,
            child_oid,
            &relation.partition_key,
            &relation.partition_key_type,
            &relation.partition_unit,
            0,
            "",
        )?;
        create_partition_indexes(conn, relation.oid, &child_relname)?;
    } else if let Some(child) = partition_child_by_value(conn, relation.oid, partition_value)? {
        validate_table_physical(conn, child.child_oid, &child.schema, &child.relname)?;
    } else {
        return Err(format!(
            "physical partition exists without catalog relation: rsduck_internal.{child_relname}"
        ));
    }

    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET status = 'active', error_message = '', dropped_at = NULL, activated_at = CURRENT_TIMESTAMP \
             WHERE parent_relid = {} AND partition_value = '{}'",
            relation.oid,
            sql_string(partition_value)
        ),
        [],
    )
    .map_err(|e| format!("repair partition metadata failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.pg_class SET status = 'active', error_message = '' WHERE oid = {child_oid}"
        ),
        [],
    )
    .map_err(|e| format!("repair partition relation status failed: {e}"))?;
    Ok(())
}


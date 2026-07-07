pub fn validate_after_start(conn: &Connection) -> Result<(), String> {
    if !catalog_exists(conn)? {
        if has_user_objects(conn)? {
            return Err(
                "rsduck catalog is missing but DuckDB already contains user objects".into(),
            );
        }
        bootstrap_fresh(conn)?;
    }

    let version: i64 = conn
        .query_row(
            "SELECT schema_version FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read catalog version failed: {e}"))?;
    if version != CATALOG_VERSION {
        return Err(format!(
            "unsupported rsduck catalog schema version: {version}, expected {CATALOG_VERSION}"
        ));
    }

    let status: String = conn
        .query_row(
            "SELECT status FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read catalog status failed: {e}"))?;
    if status != "ready" {
        return Err(format!("rsduck catalog status is not ready: {status}"));
    }

    validate_catalog_journal_state(conn)?;
    validate_catalog_integrity(conn)?;
    validate_catalog_checksum(conn)?;
    validate_physical_relations(conn)?;
    validate_partitioned_relations(conn)?;
    refresh_catalog_checksum(conn)?;
    Ok(())
}

fn validate_catalog_journal_state(conn: &Connection) -> Result<(), String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_catalog_journal \
             WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check catalog journal state failed: {e}"))?;
    if count == 0 {
        return Ok(());
    }

    let mut stmt = conn
        .prepare(
            "SELECT journal_id, mutation_type, status, error_message \
             FROM rsduck_catalog.rs_catalog_journal \
             WHERE status = 'pending' \
             ORDER BY journal_id \
             LIMIT 5",
        )
        .map_err(|e| format!("prepare unfinished catalog journal summary failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query unfinished catalog journal summary failed: {e}"))?;
    let mut summaries = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read unfinished catalog journal summary failed: {e}"))?
    {
        let journal_id: i64 = row
            .get(0)
            .map_err(|e| format!("read journal id failed: {e}"))?;
        let mutation_type: String = row
            .get(1)
            .map_err(|e| format!("read journal mutation type failed: {e}"))?;
        let status: String = row
            .get(2)
            .map_err(|e| format!("read journal status failed: {e}"))?;
        let error_message: String = row
            .get(3)
            .map_err(|e| format!("read journal error message failed: {e}"))?;
        summaries.push(format!(
            "#{journal_id} {mutation_type} {status}: {error_message}"
        ));
    }

    let summary = summaries.join("; ");
    warn!("Catalog journal contains unfinished mutations recovered at startup: {summary}");
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_catalog_journal \
             SET status = 'failed', error_message = '{}', applied_at = CURRENT_TIMESTAMP \
             WHERE status = 'pending'",
            sql_string(&format!("recovered at startup: {summary}"))
        ),
        [],
    )
    .map_err(|e| format!("recover pending catalog journal failed: {e}"))?;
    refresh_catalog_checksum(conn)?;
    Ok(())
}

fn validate_catalog_integrity(conn: &Connection) -> Result<(), String> {
    ensure_catalog_count_zero(
        conn,
        "SELECT COUNT(*) \
         FROM rsduck_catalog.pg_class c \
         LEFT JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.oid IS NULL",
        "pg_class.relnamespace must reference pg_namespace",
    )?;
    ensure_catalog_count_zero(
        conn,
        "SELECT COUNT(*) \
         FROM rsduck_catalog.pg_attribute a \
         LEFT JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid \
         WHERE c.oid IS NULL",
        "pg_attribute.attrelid must reference pg_class",
    )?;
    ensure_catalog_count_zero(
        conn,
        "SELECT COUNT(*) \
         FROM rsduck_catalog.pg_attribute a \
         LEFT JOIN rsduck_catalog.pg_type t ON t.oid = a.atttypid \
         WHERE t.oid IS NULL",
        "pg_attribute.atttypid must reference pg_type",
    )?;
    ensure_catalog_count_zero(
        conn,
        "SELECT COUNT(*) FROM ( \
             SELECT c.relnamespace, lower(c.relname) AS relname \
             FROM rsduck_catalog.pg_class c \
             WHERE c.status = 'active' \
             GROUP BY c.relnamespace, lower(c.relname) \
             HAVING COUNT(*) > 1 \
         ) duplicate_relations",
        "active relation names must be unique per namespace",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) FROM rsduck_catalog.pg_depend \
             WHERE classid NOT IN ({PG_CLASS_CLASSOID}, {PG_CONSTRAINT_CLASSOID}, {PG_NAMESPACE_CLASSOID}) \
                OR refclassid NOT IN ({PG_CLASS_CLASSOID}, {PG_CONSTRAINT_CLASSOID}, {PG_NAMESPACE_CLASSOID})"
        ),
        "pg_depend classid/refclassid must reference supported catalog classes",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_class c ON c.oid = d.objid \
             WHERE d.classid = {PG_CLASS_CLASSOID} AND c.oid IS NULL"
        ),
        "pg_depend class object must reference pg_class",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_class c ON c.oid = d.refobjid \
             WHERE d.refclassid = {PG_CLASS_CLASSOID} AND c.oid IS NULL"
        ),
        "pg_depend referenced class object must reference pg_class",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_constraint con ON con.oid = d.objid \
             WHERE d.classid = {PG_CONSTRAINT_CLASSOID} AND con.oid IS NULL"
        ),
        "pg_depend constraint object must reference pg_constraint",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_constraint con ON con.oid = d.refobjid \
             WHERE d.refclassid = {PG_CONSTRAINT_CLASSOID} AND con.oid IS NULL"
        ),
        "pg_depend referenced constraint object must reference pg_constraint",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_namespace n ON n.oid = d.objid \
             WHERE d.classid = {PG_NAMESPACE_CLASSOID} AND n.oid IS NULL"
        ),
        "pg_depend namespace object must reference pg_namespace",
    )?;
    ensure_catalog_count_zero(
        conn,
        &format!(
            "SELECT COUNT(*) \
             FROM rsduck_catalog.pg_depend d \
             LEFT JOIN rsduck_catalog.pg_namespace n ON n.oid = d.refobjid \
             WHERE d.refclassid = {PG_NAMESPACE_CLASSOID} AND n.oid IS NULL"
        ),
        "pg_depend referenced namespace object must reference pg_namespace",
    )?;
    Ok(())
}

fn ensure_catalog_count_zero(conn: &Connection, sql: &str, violation: &str) -> Result<(), String> {
    let count: i64 = conn
        .query_row(sql, [], |row| row.get(0))
        .map_err(|e| format!("catalog integrity check failed: {violation}: {e}"))?;
    if count == 0 {
        Ok(())
    } else {
        Err(format!(
            "catalog integrity violation: {violation}; invalid rows={count}"
        ))
    }
}


fn validate_physical_relations(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, n.nspname, c.relname, c.relkind \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.status = 'active' AND c.relkind IN ('r', 'v', 'i') \
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare catalog physical validation failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query catalog physical validation failed: {e}"))?;
    let mut relations = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read catalog physical validation failed: {e}"))?
    {
        relations.push((
            row.get::<_, i64>(0)
                .map_err(|e| format!("read rel oid failed: {e}"))?,
            row.get::<_, String>(1)
                .map_err(|e| format!("read rel schema failed: {e}"))?,
            row.get::<_, String>(2)
                .map_err(|e| format!("read rel name failed: {e}"))?,
            row.get::<_, String>(3)
                .map_err(|e| format!("read rel kind failed: {e}"))?,
        ));
    }

    for (rel_oid, schema, relname, relkind) in relations {
        let validation = match relkind.as_str() {
            "r" => validate_table_physical(conn, rel_oid, &schema, &relname),
            "v" => validate_view_physical(conn, rel_oid, &schema, &relname),
            "i" => validate_index_physical(conn, rel_oid, &schema, &relname),
            _ => Ok(()),
        };

        if let Err(reason) = validation {
            warn!(
                "Catalog relation unavailable after startup validation: {}.{}: {}",
                schema, relname, reason
            );
            mark_relation_unavailable(conn, rel_oid, &reason)?;
        }
    }

    Ok(())
}

fn validate_partitioned_relations(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, n.nspname, c.relname \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.status = 'active' AND c.relkind = 'p' \
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare partitioned relation validation failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partitioned relation validation failed: {e}"))?;
    let mut parents = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partitioned relation validation failed: {e}"))?
    {
        parents.push((
            row.get::<_, i64>(0)
                .map_err(|e| format!("read partitioned rel oid failed: {e}"))?,
            row.get::<_, String>(1)
                .map_err(|e| format!("read partitioned rel schema failed: {e}"))?,
            row.get::<_, String>(2)
                .map_err(|e| format!("read partitioned rel name failed: {e}"))?,
        ));
    }

    for (parent_oid, schema, relname) in parents {
        if let Err(reason) = validate_partitioned_relation(conn, parent_oid, &schema, &relname) {
            warn!(
                "Catalog partitioned relation unavailable after startup validation: {}.{}: {}",
                schema, relname, reason
            );
            mark_relation_unavailable(conn, parent_oid, &reason)?;
        }
    }
    Ok(())
}

fn validate_partitioned_relation(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let partitions = active_partition_children(conn, parent_oid)?;
    if partitions.is_empty() {
        return Err("managed partitioned table has no active partitions".into());
    }

    let mut active_physical = Vec::with_capacity(partitions.len());
    for partition in &partitions {
        if partition.child_status != "active" {
            let reason = format!(
                "active partition child is not active: {}.{} status={}",
                partition.schema, partition.relname, partition.child_status
            );
            mark_partition_failed(conn, parent_oid, partition.child_oid, &reason)?;
            return Err(reason);
        }
        if let Err(reason) = validate_table_physical(
            conn,
            partition.child_oid,
            &partition.schema,
            &partition.relname,
        ) {
            mark_relation_unavailable(conn, partition.child_oid, &reason)?;
            mark_partition_failed(conn, parent_oid, partition.child_oid, &reason)?;
            return Err(format!(
                "active partition child unavailable: {}.{}: {reason}",
                partition.schema, partition.relname
            ));
        }
        active_physical.push((partition.schema.as_str(), partition.relname.as_str()));
    }

    let expected_sql = partition_entrypoint_sql(schema, relname, &active_physical);
    let generated_sql: String = conn
        .query_row(
            &format!(
                "SELECT generated_sql FROM rsduck_catalog.rs_relation_ext WHERE relid = {parent_oid}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read partition entrypoint SQL failed: {e}"))?;

    if generated_sql.trim() != expected_sql {
        rebuild_partition_entrypoint(conn, parent_oid, &expected_sql)?;
    } else if validate_view_physical(conn, parent_oid, schema, relname).is_err() {
        rebuild_partition_entrypoint(conn, parent_oid, &expected_sql)?;
    }
    validate_view_physical(conn, parent_oid, schema, relname)?;
    sync_partition_dependencies(conn, parent_oid, &partitions)?;
    Ok(())
}

#[derive(Debug)]
struct ActivePartitionChild {
    child_oid: i64,
    schema: String,
    relname: String,
    is_null_partition: bool,
    child_status: String,
}

fn active_partition_children(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<ActivePartitionChild>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.child_relid, n.nspname, c.relname, p.is_null_partition, c.status \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} AND p.status = 'active' \
             ORDER BY p.is_null_partition, p.partition_value"
        ))
        .map_err(|e| format!("prepare active partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query active partition lookup failed: {e}"))?;
    let mut partitions = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read active partition lookup failed: {e}"))?
    {
        partitions.push(ActivePartitionChild {
            child_oid: row
                .get(0)
                .map_err(|e| format!("read active partition child oid failed: {e}"))?,
            schema: row
                .get(1)
                .map_err(|e| format!("read active partition schema failed: {e}"))?,
            relname: row
                .get(2)
                .map_err(|e| format!("read active partition relation failed: {e}"))?,
            is_null_partition: row
                .get(3)
                .map_err(|e| format!("read active partition null flag failed: {e}"))?,
            child_status: row
                .get(4)
                .map_err(|e| format!("read active partition child status failed: {e}"))?,
        });
    }
    Ok(partitions)
}

fn rebuild_partition_entrypoint(
    conn: &Connection,
    parent_oid: i64,
    expected_sql: &str,
) -> Result<(), String> {
    conn.execute(expected_sql, [])
        .map_err(|e| format!("rebuild partition entrypoint failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation_ext \
             SET generated_sql = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE relid = {parent_oid}",
            sql_string(expected_sql)
        ),
        [],
    )
    .map_err(|e| format!("record rebuilt partition entrypoint failed: {e}"))?;
    Ok(())
}

fn mark_partition_failed(
    conn: &Connection,
    parent_oid: i64,
    child_oid: i64,
    reason: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET status = 'failed', error_message = '{}' \
             WHERE parent_relid = {parent_oid} AND child_relid = {child_oid}",
            sql_string(reason)
        ),
        [],
    )
    .map_err(|e| format!("mark partition failed failed: {e}"))?;
    Ok(())
}

fn validate_table_physical(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let count = count_duckdb_relation(conn, "duckdb_tables()", "table_name", schema, relname)?;
    if count == 0 {
        return Err("missing DuckDB physical table".into());
    }
    validate_catalog_columns_match_duckdb(conn, rel_oid, schema, relname)
}

fn validate_view_physical(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let count = count_duckdb_relation(conn, "duckdb_views()", "view_name", schema, relname)?;
    if count == 0 {
        return Err("missing DuckDB physical view".into());
    }
    validate_catalog_columns_match_duckdb(conn, rel_oid, schema, relname)
}

fn validate_index_physical(
    conn: &Connection,
    index_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    if let Some(parent_oid) = partitioned_index_parent(conn, index_oid)? {
        let specs = partition_index_specs(conn, parent_oid)?
            .into_iter()
            .filter(|spec| spec.index_oid == index_oid)
            .collect::<Vec<_>>();
        let spec = specs
            .first()
            .ok_or_else(|| format!("missing partitioned index metadata: {schema}.{relname}"))?;
        for partition in active_partition_children(conn, parent_oid)? {
            let child_index = partition_index_name(&partition.relname, &spec.index_name);
            if !duckdb_index_exists(conn, "rsduck_internal", &child_index)? {
                return Err(format!(
                    "missing DuckDB physical partition index: rsduck_internal.{child_index}"
                ));
            }
        }
        return Ok(());
    }

    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM duckdb_indexes() \
                 WHERE schema_name = '{}' AND index_name = '{}'",
                sql_string(schema),
                sql_string(relname)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("query DuckDB physical index failed: {e}"))?;
    if count == 0 {
        return Err("missing DuckDB physical index".into());
    }
    Ok(())
}

fn validate_catalog_columns_match_duckdb(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let catalog = catalog_columns(conn, rel_oid)?;
    let physical = load_duckdb_columns(conn, schema, relname)?;
    if catalog.len() != physical.len() {
        return Err(format!(
            "column count mismatch: catalog={}, duckdb={}",
            catalog.len(),
            physical.len()
        ));
    }
    for (catalog_column, physical_column) in catalog.iter().zip(physical.iter()) {
        if !catalog_column
            .name
            .eq_ignore_ascii_case(&physical_column.name)
            || catalog_column.pg_type_oid != physical_column.pg_type_oid
        {
            return Err(format!(
                "column mismatch at catalog attnum {}: catalog={} duckdb={}",
                catalog_column.attnum, catalog_column.name, physical_column.name
            ));
        }
    }
    Ok(())
}

fn count_duckdb_relation(
    conn: &Connection,
    table_function: &str,
    name_column: &str,
    schema: &str,
    relname: &str,
) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {table_function} \
             WHERE schema_name = '{}' AND {name_column} = '{}' AND internal = FALSE",
            sql_string(schema),
            sql_string(relname)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("query DuckDB physical relation failed: {e}"))
}

fn mark_relation_unavailable(conn: &Connection, rel_oid: i64, reason: &str) -> Result<(), String> {
    let error_message = relation_unavailable_message(rel_oid, reason);
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.pg_class \
             SET status = 'unavailable', error_message = '{}' \
             WHERE oid = {rel_oid}",
            sql_string(&error_message)
        ),
        [],
    )
    .map_err(|e| format!("mark relation unavailable failed: {e}"))?;
    Ok(())
}

fn relation_unavailable_message(rel_oid: i64, reason: &str) -> String {
    if reason.contains("RS-CATALOG-") {
        reason.to_string()
    } else {
        format!("RS-CATALOG-{rel_oid}: {reason}")
    }
}


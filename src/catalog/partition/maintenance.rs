fn expire_old_partitions(
    conn: &Connection,
    relation: &PartitionedRelation,
) -> Result<usize, String> {
    if relation.retention_count <= 0 {
        return Ok(0);
    }
    let partitions = retention_partitions(conn, relation.oid)?;
    let retention_count = relation.retention_count as usize;
    if partitions.len() <= retention_count {
        return Ok(0);
    }

    let expire_count = partitions.len() - retention_count;
    for partition in partitions.into_iter().take(expire_count) {
        expire_partition(conn, relation.oid, partition)?;
    }
    Ok(expire_count)
}

fn run_partition_maintenance(conn: &Connection, sql: &str) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let journal_id = insert_journal(conn, "run_partition_maintenance", 0, sql)?;
        let mut expired = 0usize;
        for relation in partitioned_relations(conn)? {
            expired += expire_old_partitions(conn, &relation)?;
            refresh_partition_entrypoint(conn, relation.oid, &relation.schema, &relation.name)?;
        }
        finish_journal(conn, journal_id)?;
        Ok(expired)
    })
}

fn partitioned_relations(conn: &Connection) -> Result<Vec<PartitionedRelation>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT n.nspname, c.relname \
             FROM rsduck_catalog.pg_class c \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid \
             WHERE c.status = 'active' \
               AND c.relkind = 'p' \
               AND ext.managed_kind = 'range_partitioned_table' \
             ORDER BY n.nspname, c.relname",
        )
        .map_err(|e| format!("prepare partitioned relation list failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partitioned relation list failed: {e}"))?;
    let mut relations = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partitioned relation list failed: {e}"))?
    {
        let schema: String = row
            .get(0)
            .map_err(|e| format!("read partitioned relation schema failed: {e}"))?;
        let table: String = row
            .get(1)
            .map_err(|e| format!("read partitioned relation name failed: {e}"))?;
        if let Some(relation) = partitioned_relation(conn, &schema, &table)? {
            relations.push(relation);
        }
    }
    Ok(relations)
}


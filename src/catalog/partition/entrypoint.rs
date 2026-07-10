use super::*;

pub(in crate::catalog) fn partition_entrypoint_sql(
    schema: &str,
    table: &str,
    partitions: &[(&str, &str)],
) -> String {
    let selects = partitions
        .iter()
        .map(|(partition_schema, partition_name)| {
            format!(
                "SELECT * FROM {}",
                quote_qualified(partition_schema, partition_name)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    format!(
        "CREATE OR REPLACE VIEW {} AS {selects}",
        quote_qualified(schema, table)
    )
}

pub(in crate::catalog) fn empty_partition_entrypoint_sql_from_create_table(
    schema: &str,
    table: &str,
    create_table: &CreateTable,
) -> String {
    let selects = create_table
        .columns
        .iter()
        .map(|column| {
            format!(
                "CAST(NULL AS {}) AS {}",
                column.data_type,
                quote_ident(&column.name.value)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE OR REPLACE VIEW {} AS SELECT {selects} WHERE FALSE",
        quote_qualified(schema, table)
    )
}

pub(in crate::catalog) fn partition_entrypoint_sql_from_catalog(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    partitions: &[ActivePartitionChild],
) -> Result<String, String> {
    if !partitions.is_empty() {
        let active_physical = partitions
            .iter()
            .map(|partition| (partition.schema.as_str(), partition.relname.as_str()))
            .collect::<Vec<_>>();
        return Ok(partition_entrypoint_sql(schema, table, &active_physical));
    }

    let columns = catalog_columns(conn, parent_oid)?;
    let selects = columns
        .iter()
        .map(|column| {
            Ok(format!(
                "CAST(NULL AS {}) AS {}",
                duckdb_type_for_type_id(conn, column.type_id)?,
                quote_ident(&column.name)
            ))
        })
        .collect::<Result<Vec<_>, String>>()?
        .join(", ");
    Ok(format!(
        "CREATE OR REPLACE VIEW {} AS SELECT {selects} WHERE FALSE",
        quote_qualified(schema, table)
    ))
}

pub(in crate::catalog) fn refresh_partition_entrypoint(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let partitions = active_partition_children(conn, parent_oid)?;
    let sql =
        partition_entrypoint_sql_from_catalog(conn, parent_oid, schema, relname, &partitions)?;
    rebuild_partition_entrypoint(conn, parent_oid, &sql)?;
    sync_partition_dependencies(conn, parent_oid, &partitions)?;
    Ok(())
}

pub(in crate::catalog) fn sync_partition_dependencies(
    conn: &Connection,
    parent_oid: i64,
    partitions: &[ActivePartitionChild],
) -> Result<(), String> {
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_dependency \
             WHERE classid = {OBJECT_RELATION_KIND} AND objid = {parent_oid} \
               AND refclassid = {OBJECT_RELATION_KIND}"
        ),
        [],
    )
    .map_err(|e| format!("delete partition dependencies failed: {e}"))?;
    for partition in partitions {
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_dependency(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({OBJECT_RELATION_KIND}, {parent_oid}, 0, {OBJECT_RELATION_KIND}, {}, 0, 'n')",
                partition.child_oid
            ),
            [],
        )
        .map_err(|e| format!("write partition dependency failed: {e}"))?;
    }
    Ok(())
}

pub(in crate::catalog) fn update_partition_stats(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
    inserted_rows: i64,
    route_ts: Option<NaiveDateTime>,
) -> Result<(), String> {
    let ts_update = route_ts
        .map(|dt| {
            format!(
                ", min_ts = CASE WHEN min_ts IS NULL OR TIMESTAMP '{dt}' < min_ts THEN TIMESTAMP '{dt}' ELSE min_ts END, \
                 max_ts = CASE WHEN max_ts IS NULL OR TIMESTAMP '{dt}' > max_ts THEN TIMESTAMP '{dt}' ELSE max_ts END"
            )
        })
        .unwrap_or_default();
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET row_count = row_count + {inserted_rows}{ts_update} \
             WHERE parent_relid = {parent_oid} AND partition_value = '{}'",
            sql_string(partition_value)
        ),
        [],
    )
    .map_err(|e| format!("update partition stats failed: {e}"))?;
    Ok(())
}

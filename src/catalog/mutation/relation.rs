use super::*;

pub(in crate::catalog) fn insert_relation_rows(
    conn: &Connection,
    rel_oid: i64,
    type_oid: i64,
    schema: &str,
    relation: &str,
    relkind: &str,
    managed_kind: &str,
    visibility: &str,
    generated_sql: &str,
    columns: &[CatalogColumn],
    owner_user_id: i64,
) -> Result<(), String> {
    let namespace_oid = namespace_oid(conn, schema)?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_type(oid, typname, typnamespace, typowner, typlen, \
             typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type) \
             VALUES ({type_oid}, '{}', {namespace_oid}, {owner_user_id}, -1, FALSE, 'c', 'C', TRUE, {rel_oid}, 0, 0, 'STRUCT')",
            sql_string(relation)
        ),
        [],
    )
    .map_err(|e| format!("write relation row type failed: {e}"))?;

    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_relation(oid, relname, relnamespace, reltype, relowner, \
             relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message) \
             VALUES ({rel_oid}, '{}', {namespace_oid}, {type_oid}, {owner_user_id}, '{}', 'p', {}, 0, FALSE, FALSE, '', '', 'active', '')",
            sql_string(relation),
            sql_string(relkind),
            columns.len()
        ),
        [],
    )
    .map_err(|e| format!("write rs_relation failed: {e}"))?;

    for column in columns {
        insert_attribute_row(conn, rel_oid, column)?;
    }

    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_relation_ext(relid, managed_kind, storage_mode, visibility, \
             partition_key, partition_key_type, partition_unit, retention_count, generated_sql, properties_json, created_at, updated_at) \
             VALUES ({rel_oid}, '{}', 'memory', '{}', '', '', '', 0, '{}', '{{}}', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            sql_string(managed_kind),
            sql_string(visibility),
            sql_string(generated_sql)
        ),
        [],
    )
    .map_err(|e| format!("write rs_relation_ext failed: {e}"))?;

    Ok(())
}

pub(in crate::catalog) fn insert_attribute_row(
    conn: &Connection,
    rel_oid: i64,
    column: &CatalogColumn,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_column(attrelid, attname, atttypid, attnum, atttypmod, \
             attnotnull, atthasdef, attisdropped, attidentity, attgenerated, attoptions) \
             VALUES ({rel_oid}, '{}', {}, {}, -1, {}, {}, FALSE, '', '', '')",
            sql_string(&column.name),
            column.type_id,
            column.attnum,
            sql_bool(column.not_null),
            sql_bool(column.default_expr.is_some())
        ),
        [],
    )
    .map_err(|e| format!("write rs_column failed: {e}"))?;

    if let Some(default_expr) = &column.default_expr {
        let default_oid = allocate_oid(conn)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_column_default(oid, adrelid, adnum, adbin) \
                 VALUES ({default_oid}, {rel_oid}, {}, '{}')",
                column.attnum,
                sql_string(default_expr)
            ),
            [],
        )
        .map_err(|e| format!("write rs_column_default failed: {e}"))?;
    }
    Ok(())
}

pub(in crate::catalog) fn update_partition_relation_ext(
    conn: &Connection,
    rel_oid: i64,
    partition_key: &str,
    partition_key_type: &str,
    partition_unit: &str,
    retention_count: i32,
    generated_sql: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation_ext \
             SET partition_key = '{}', partition_key_type = '{}', partition_unit = '{}', \
                 retention_count = {retention_count}, generated_sql = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE relid = {rel_oid}",
            sql_string(partition_key),
            sql_string(partition_key_type),
            sql_string(partition_unit),
            sql_string(generated_sql)
        ),
        [],
    )
    .map_err(|e| format!("update partition relation extension failed: {e}"))?;
    Ok(())
}

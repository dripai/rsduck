use super::*;

pub(in crate::catalog) fn physical_partition_name(parent: &str, partition_value: &str) -> String {
    let suffix = partition_value.trim_start_matches('_');
    format!("{parent}_{suffix}")
}

pub(in crate::catalog) fn physical_partition_create_from_catalog_sql(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    relation: &str,
    columns: &[CatalogColumn],
) -> Result<String, String> {
    let mut column_defs = Vec::with_capacity(columns.len());
    for column in columns {
        let mut definition = format!(
            "{} {}",
            quote_ident(&column.name),
            duckdb_type_for_type_id(conn, column.type_id)?
        );
        if column.not_null {
            definition.push_str(" NOT NULL");
        }
        if let Some(default_expr) = &column.default_expr {
            definition.push_str(" DEFAULT ");
            definition.push_str(default_expr);
        }
        column_defs.push(definition);
    }
    column_defs.extend(physical_partition_constraints_from_catalog(
        conn, parent_oid,
    )?);
    Ok(format!(
        "CREATE TABLE {} ({})",
        quote_qualified(schema, relation),
        column_defs.join(", ")
    ))
}

pub(in crate::catalog) fn physical_partition_constraints_from_catalog(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT conname, contype, conkey, confrelid, confkey, conbin \
             FROM rsduck_catalog.rs_constraint \
             WHERE conrelid = {parent_oid} \
             ORDER BY oid"
        ))
        .map_err(|e| format!("prepare partition constraint lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition constraint lookup failed: {e}"))?;
    let mut constraints = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition constraint lookup failed: {e}"))?
    {
        let conname: String = row
            .get(0)
            .map_err(|e| format!("read constraint name failed: {e}"))?;
        let contype: String = row
            .get(1)
            .map_err(|e| format!("read constraint type failed: {e}"))?;
        let conkey: String = row
            .get(2)
            .map_err(|e| format!("read constraint key failed: {e}"))?;
        let confrelid: i64 = row
            .get(3)
            .map_err(|e| format!("read constraint foreign relid failed: {e}"))?;
        let confkey: String = row
            .get(4)
            .map_err(|e| format!("read constraint foreign key failed: {e}"))?;
        let conbin: String = row
            .get(5)
            .map_err(|e| format!("read constraint expression failed: {e}"))?;
        let prefix = format!("CONSTRAINT {}", quote_ident(&conname));
        match contype.as_str() {
            "p" => {
                let columns = constraint_column_list(conn, parent_oid, &conkey)?;
                constraints.push(format!("{prefix} PRIMARY KEY ({columns})"));
            }
            "u" => {
                let columns = constraint_column_list(conn, parent_oid, &conkey)?;
                constraints.push(format!("{prefix} UNIQUE ({columns})"));
            }
            "c" => constraints.push(format!("{prefix} CHECK ({conbin})")),
            "f" => {
                let columns = constraint_column_list(conn, parent_oid, &conkey)?;
                let ref_columns = constraint_column_list(conn, confrelid, &confkey)?;
                let (ref_schema, ref_table) = relation_name_by_oid(conn, confrelid)?;
                constraints.push(format!(
                    "{prefix} FOREIGN KEY ({columns}) REFERENCES {} ({ref_columns})",
                    quote_qualified(&ref_schema, &ref_table)
                ));
            }
            _ => {}
        }
    }
    Ok(constraints)
}

pub(in crate::catalog) fn constraint_column_list(
    conn: &Connection,
    rel_oid: i64,
    attnums: &str,
) -> Result<String, String> {
    let mut columns = Vec::new();
    for attnum in parse_attnum_list(attnums)? {
        let name = column_name_by_attnum(conn, rel_oid, attnum)?;
        columns.push(quote_ident(&name));
    }
    Ok(columns.join(", "))
}

pub(in crate::catalog) fn parse_attnum_list(attnums: &str) -> Result<Vec<i32>, String> {
    if attnums.trim().is_empty() {
        return Ok(Vec::new());
    }
    attnums
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<i32>()
                .map_err(|_| format!("invalid attnum in constraint key: {attnums}"))
        })
        .collect()
}

pub(in crate::catalog) fn create_range_partition(
    conn: &Connection,
    relation: &PartitionedRelation,
    partition_value: &str,
) -> Result<String, String> {
    let bounds = partition_bounds(partition_value, &relation.partition_unit)?;
    let child_relname = physical_partition_name(&relation.name, partition_value);
    if relation_exists(conn, "rsduck_internal", &child_relname)? {
        return Err(format!(
            "managed physical partition relation already exists: rsduck_internal.{child_relname}"
        ));
    }

    let child_oid = allocate_oid(conn)?;
    let child_type_oid = allocate_oid(conn)?;
    let create_sql = physical_partition_create_from_catalog_sql(
        conn,
        relation.oid,
        "rsduck_internal",
        &child_relname,
        &relation.columns,
    )?;
    conn.execute(&create_sql, [])
        .map_err(|e| format!("execute DuckDB CREATE partition failed: {e}"))?;
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
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation \
             SET relispartition = TRUE, relpartbound = '{}' \
             WHERE oid = {child_oid}",
            sql_string(&format!("[{}, {})", bounds.lower_bound, bounds.upper_bound))
        ),
        [],
    )
    .map_err(|e| format!("mark physical partition rs_relation failed: {e}"))?;
    update_partition_relation_ext(
        conn,
        child_oid,
        &relation.partition_key,
        &relation.partition_key_type,
        &relation.partition_unit,
        0,
        "",
    )?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_partition(parent_relid, child_relid, partition_value, \
             partition_unit, lower_bound, upper_bound, is_null_partition, status, row_count, min_ts, \
             max_ts, checksum, created_at, activated_at, dropped_at, error_message) \
             VALUES ({}, {child_oid}, '{}', '{}', TIMESTAMP '{}', TIMESTAMP '{}', FALSE, 'active', \
             0, NULL, NULL, '', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, '')",
            relation.oid,
            sql_string(&bounds.value),
            sql_string(&relation.partition_unit),
            bounds.lower_bound,
            bounds.upper_bound
        ),
        [],
    )
    .map_err(|e| format!("write range partition metadata failed: {e}"))?;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_dependency(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
             VALUES ({OBJECT_RELATION_KIND}, {}, 0, {OBJECT_RELATION_KIND}, {child_oid}, 0, 'n')",
            relation.oid
        ),
        [],
    )
    .map_err(|e| format!("write range partition dependency failed: {e}"))?;
    create_partition_indexes(conn, relation.oid, &child_relname)?;
    Ok(child_relname)
}

#[derive(Debug)]
pub(in crate::catalog) struct RetentionPartition {
    pub(in crate::catalog) partition_value: String,
    pub(in crate::catalog) schema: String,
    pub(in crate::catalog) relname: String,
    pub(in crate::catalog) meta: RelationMeta,
}

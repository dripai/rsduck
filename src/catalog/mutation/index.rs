use super::*;

pub(in crate::catalog) fn create_index_relation(
    conn: &Connection,
    create_index: &CreateIndex,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_index.name.is_none() {
        return Err("CREATE INDEX requires an explicit index name".into());
    }
    if create_index.predicate.is_some() {
        return Err("partial index is not supported by rsduck catalog".into());
    }
    if !create_index.include.is_empty() {
        return Err("index INCLUDE columns are not supported by rsduck catalog".into());
    }

    let (table_schema, table_name) = relation_name(&create_index.table_name)?;
    reject_reserved_schema(&table_schema)?;

    let index_name = create_index.name.as_ref().expect("index name checked");
    let (index_schema, index_relname) = relation_name_with_default(index_name, &table_schema)?;
    if index_schema != table_schema {
        return Err("index schema must match table schema".into());
    }

    let index_column_names = simple_index_column_names(&create_index.columns)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &index_schema, &index_relname)? {
            if create_index.if_not_exists {
                return Ok(0);
            }
            return Err(format!(
                "relation already exists: {index_schema}.{index_relname}"
            ));
        }

        let table_oid = relation_oid(conn, &table_schema, &table_name)?;
        let table_kind = relation_kind(conn, table_oid)?;
        if table_kind != "r" && table_kind != "p" {
            return Err(format!(
                "CREATE INDEX only supports ordinary or partitioned tables, got relkind={table_kind}"
            ));
        }

        let table_columns = catalog_columns(conn, table_oid)?;
        let mut indkey = Vec::with_capacity(index_column_names.len());
        for column_name in &index_column_names {
            let attnum = table_columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(column_name))
                .map(|column| column.attnum)
                .ok_or_else(|| format!("index references unknown column: {column_name}"))?;
            indkey.push(attnum.to_string());
        }

        let index_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_index", index_oid, sql)?;
        if table_kind == "p" {
            create_partition_indexes_from_columns(
                conn,
                table_oid,
                &index_relname,
                &index_column_names,
                create_index.unique,
            )?;
        } else {
            conn.execute(sql, [])
                .map_err(|e| format!("execute DuckDB CREATE INDEX failed: {e}"))?;
        }

        let namespace_oid = namespace_oid(conn, &index_schema)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_relation(oid, relname, relnamespace, reltype, relowner, \
                 relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message) \
                 VALUES ({index_oid}, '{}', {namespace_oid}, 0, {owner_user_id}, 'i', 'p', {}, 0, FALSE, FALSE, '', '', 'active', '')",
                sql_string(&index_relname),
                index_column_names.len()
            ),
            [],
        )
        .map_err(|e| format!("write index pg_class failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_index(indexrelid, indrelid, indnatts, indnkeyatts, \
                 indisunique, indisprimary, indisvalid, indkey, indexprs, indpred) \
                 VALUES ({index_oid}, {table_oid}, {}, {}, {}, FALSE, TRUE, '{}', '', '')",
                index_column_names.len(),
                index_column_names.len(),
                sql_bool(create_index.unique),
                sql_string(&indkey.join(","))
            ),
            [],
        )
        .map_err(|e| format!("write pg_index failed: {e}"))?;

        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_relation SET relhasindex = TRUE WHERE oid = {table_oid}"
            ),
            [],
        )
        .map_err(|e| format!("update table relhasindex failed: {e}"))?;

        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_dependency(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({OBJECT_RELATION_KIND}, {index_oid}, 0, {OBJECT_RELATION_KIND}, {table_oid}, 0, 'n')"
            ),
            [],
        )
        .map_err(|e| format!("write index dependency failed: {e}"))?;

        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

#[derive(Debug)]
pub(in crate::catalog) struct PartitionIndexSpec {
    pub(in crate::catalog) index_oid: i64,
    pub(in crate::catalog) index_name: String,
    pub(in crate::catalog) columns: Vec<String>,
    pub(in crate::catalog) unique: bool,
}

pub(in crate::catalog) fn create_partition_indexes_from_columns(
    conn: &Connection,
    parent_oid: i64,
    index_name: &str,
    columns: &[String],
    unique: bool,
) -> Result<(), String> {
    let spec = PartitionIndexSpec {
        index_oid: 0,
        index_name: index_name.to_string(),
        columns: columns.to_vec(),
        unique,
    };
    for partition in active_partition_children(conn, parent_oid)? {
        create_partition_index(conn, &spec, &partition.relname)?;
    }
    Ok(())
}

pub(in crate::catalog) fn create_partition_indexes(
    conn: &Connection,
    parent_oid: i64,
    child_relname: &str,
) -> Result<(), String> {
    for spec in partition_index_specs(conn, parent_oid)? {
        create_partition_index(conn, &spec, child_relname)?;
    }
    Ok(())
}

pub(in crate::catalog) fn create_partition_index(
    conn: &Connection,
    spec: &PartitionIndexSpec,
    child_relname: &str,
) -> Result<(), String> {
    let child_index = partition_index_name(child_relname, &spec.index_name);
    if duckdb_index_exists(conn, "rsduck_internal", &child_index)? {
        return Ok(());
    }
    let unique = if spec.unique { "UNIQUE " } else { "" };
    let columns = spec
        .columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!(
            "CREATE {unique}INDEX {} ON {} ({columns})",
            quote_ident(&child_index),
            quote_qualified("rsduck_internal", child_relname)
        ),
        [],
    )
    .map_err(|e| {
        format!("execute DuckDB CREATE partition index rsduck_internal.{child_index} failed: {e}")
    })?;
    Ok(())
}

pub(in crate::catalog) fn partition_index_specs(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<PartitionIndexSpec>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT i.indexrelid, c.relname, i.indisunique, i.indkey \
             FROM rsduck_catalog.rs_index i \
             JOIN rsduck_catalog.rs_relation c ON c.oid = i.indexrelid \
             WHERE i.indrelid = {parent_oid} AND c.status = 'active' \
             ORDER BY i.indexrelid"
        ))
        .map_err(|e| format!("prepare partition index lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition index lookup failed: {e}"))?;
    let mut specs = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition index lookup failed: {e}"))?
    {
        let index_oid: i64 = row
            .get(0)
            .map_err(|e| format!("read partition index oid failed: {e}"))?;
        let index_name: String = row
            .get(1)
            .map_err(|e| format!("read partition index name failed: {e}"))?;
        let unique: bool = row
            .get(2)
            .map_err(|e| format!("read partition index unique flag failed: {e}"))?;
        let indkey: String = row
            .get(3)
            .map_err(|e| format!("read partition index key failed: {e}"))?;
        let mut columns = Vec::new();
        for attnum in parse_attnum_list(&indkey)? {
            columns.push(column_name_by_attnum(conn, parent_oid, attnum)?);
        }
        specs.push(PartitionIndexSpec {
            index_oid,
            index_name,
            columns,
            unique,
        });
    }
    Ok(specs)
}

pub(in crate::catalog) fn partitioned_index_parent(
    conn: &Connection,
    index_oid: i64,
) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT i.indrelid \
             FROM rsduck_catalog.rs_index i \
             JOIN rsduck_catalog.rs_relation c ON c.oid = i.indrelid \
             WHERE i.indexrelid = {index_oid} AND c.relkind = 'p'"
        ))
        .map_err(|e| format!("prepare partitioned index parent lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partitioned index parent lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read partitioned index parent lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read partitioned index parent oid failed: {e}"))
}

pub(in crate::catalog) fn duckdb_index_exists(
    conn: &Connection,
    schema: &str,
    index_name: &str,
) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM duckdb_indexes() \
                 WHERE schema_name = '{}' AND index_name = '{}'",
                sql_string(schema),
                sql_string(index_name)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("query DuckDB index failed: {e}"))?;
    Ok(count > 0)
}

pub(in crate::catalog) fn partition_index_name(
    child_relname: &str,
    parent_index_name: &str,
) -> String {
    format!("{child_relname}__{parent_index_name}")
}

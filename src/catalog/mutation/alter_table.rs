use super::*;

pub(in crate::catalog) fn alter_table_relation(
    conn: &Connection,
    alter_table: &AlterTable,
    sql: &str,
    _owner_user_id: i64,
) -> Result<usize, String> {
    let (schema, table) = relation_name(&alter_table.name)?;
    reject_reserved_schema(&schema)?;
    if alter_table.operations.len() != 1 {
        return Err("ALTER TABLE currently supports exactly one operation".into());
    }

    match &alter_table.operations[0] {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if column_position.is_some() {
                return Err(
                    "ALTER TABLE ADD COLUMN position is not supported by rsduck catalog".into(),
                );
            }
            validate_column_def_type(column_def)?;

            run_catalog_tx(conn, || {
                let rel_oid = relation_oid(conn, &schema, &table)?;
                let relkind = relation_kind(conn, rel_oid)?;
                if column_exists(conn, rel_oid, &column_def.name.value)? {
                    if *if_not_exists {
                        return Ok(0);
                    }
                    return Err(format!(
                        "column already exists: {}.{}.{}",
                        schema, table, column_def.name
                    ));
                }

                let journal_id = insert_journal(conn, "alter_table_add_column", rel_oid, sql)?;
                if relkind == "p" {
                    alter_partitioned_table_add_column(
                        conn,
                        rel_oid,
                        &schema,
                        &table,
                        &column_def.to_string(),
                    )?;
                    finish_journal(conn, journal_id)?;
                    return Ok(0);
                }
                if relkind != "r" {
                    return Err(format!(
                        "ALTER TABLE ADD COLUMN only supports ordinary or partitioned tables, got relkind={relkind}"
                    ));
                }
                conn.execute(sql, [])
                    .map_err(|e| format!("execute DuckDB ALTER TABLE ADD COLUMN failed: {e}"))?;
                let physical_columns = load_duckdb_columns(conn, &schema, &table)?;
                let mut column = physical_columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(&column_def.name.value))
                    .cloned()
                    .ok_or_else(|| {
                        format!("DuckDB did not expose added column: {}", column_def.name)
                    })?;
                column.attnum = next_attribute_num(conn, rel_oid)?;
                insert_attribute_row(conn, rel_oid, &column)?;
                set_relnatts_to_active_attribute_count(conn, rel_oid)?;
                finish_journal(conn, journal_id)?;
                Ok(0)
            })
        }
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            if drop_behavior.is_some() {
                return Err("ALTER TABLE DROP COLUMN CASCADE/RESTRICT is not supported".into());
            }
            run_catalog_tx(conn, || {
                let rel_oid = relation_oid(conn, &schema, &table)?;
                let journal_id = insert_journal(conn, "alter_table_drop_column", rel_oid, sql)?;
                alter_table_drop_columns(conn, rel_oid, &schema, &table, column_names, *if_exists)?;
                finish_journal(conn, journal_id)?;
                Ok(0)
            })
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => run_catalog_tx(conn, || {
            let rel_oid = relation_oid(conn, &schema, &table)?;
            let journal_id = insert_journal(conn, "alter_table_rename_column", rel_oid, sql)?;
            alter_table_rename_column(
                conn,
                rel_oid,
                &schema,
                &table,
                &old_column_name.value,
                &new_column_name.value,
                sql,
            )?;
            finish_journal(conn, journal_id)?;
            Ok(0)
        }),
        AlterTableOperation::AlterColumn {
            column_name,
            op: AlterColumnOperation::SetDataType { .. },
        } => run_catalog_tx(conn, || {
            let rel_oid = relation_oid(conn, &schema, &table)?;
            let journal_id = insert_journal(conn, "alter_table_set_column_type", rel_oid, sql)?;
            alter_table_set_column_type(
                conn,
                rel_oid,
                &schema,
                &table,
                &column_name.value,
                &alter_table.operations[0].to_string(),
                sql,
            )?;
            finish_journal(conn, journal_id)?;
            Ok(0)
        }),
        _ => Err("unsupported ALTER TABLE operation for rsduck catalog".into()),
    }
}

pub(in crate::catalog) fn alter_table_rename_column(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    old_name: &str,
    new_name: &str,
    sql: &str,
) -> Result<(), String> {
    if old_name.eq_ignore_ascii_case(new_name) {
        return Err("ALTER TABLE RENAME COLUMN requires distinct column names".into());
    }
    let old_attnum = column_attnum(conn, rel_oid, old_name)?
        .ok_or_else(|| format!("column does not exist in catalog: {schema}.{table}.{old_name}"))?;
    if column_exists(conn, rel_oid, new_name)? {
        return Err(format!(
            "column already exists: {schema}.{table}.{new_name}"
        ));
    }
    ensure_no_dependent_views(conn, rel_oid, schema, table, old_name, "rename")?;

    match relation_kind(conn, rel_oid)?.as_str() {
        "r" => {
            conn.execute(sql, [])
                .map_err(|e| format!("execute DuckDB ALTER TABLE RENAME COLUMN failed: {e}"))?;
            let column = duckdb_column(conn, schema, table, new_name)?;
            sync_catalog_column(conn, rel_oid, old_attnum, &column, new_name)
        }
        "p" => alter_partitioned_table_rename_column(
            conn, rel_oid, schema, table, old_name, new_name, old_attnum,
        ),
        relkind => Err(format!(
            "ALTER TABLE RENAME COLUMN only supports ordinary or partitioned tables, got relkind={relkind}"
        )),
    }
}

pub(in crate::catalog) fn alter_table_set_column_type(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    column_name: &str,
    operation_sql: &str,
    sql: &str,
) -> Result<(), String> {
    let attnum = column_attnum(conn, rel_oid, column_name)?.ok_or_else(|| {
        format!("column does not exist in catalog: {schema}.{table}.{column_name}")
    })?;
    ensure_no_dependent_views(conn, rel_oid, schema, table, column_name, "change type")?;

    match relation_kind(conn, rel_oid)?.as_str() {
        "r" => {
            conn.execute(sql, [])
                .map_err(|e| format!("execute DuckDB ALTER TABLE SET DATA TYPE failed: {e}"))?;
            let column = duckdb_column(conn, schema, table, column_name)?;
            sync_catalog_column(conn, rel_oid, attnum, &column, column_name)
        }
        "p" => alter_partitioned_table_set_column_type(
            conn,
            rel_oid,
            schema,
            table,
            column_name,
            attnum,
            operation_sql,
        ),
        relkind => Err(format!(
            "ALTER TABLE SET DATA TYPE only supports ordinary or partitioned tables, got relkind={relkind}"
        )),
    }
}

fn alter_partitioned_table_rename_column(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    old_name: &str,
    new_name: &str,
    parent_attnum: i32,
) -> Result<(), String> {
    let partition_key = partition_key_name(conn, parent_oid)?;
    let children = active_partition_children(conn, parent_oid)?;
    if children.is_empty() {
        return Err("partitioned table has no active physical partitions".into());
    }

    for child in &children {
        let child_attnum = column_attnum(conn, child.child_oid, old_name)?.ok_or_else(|| {
            format!(
                "partition column does not exist in catalog: {}.{}.{}",
                child.schema, child.relname, old_name
            )
        })?;
        conn.execute(
            &format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                quote_qualified(&child.schema, &child.relname),
                quote_ident(old_name),
                quote_ident(new_name)
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB ALTER TABLE RENAME COLUMN on partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;
        let column = duckdb_column(conn, &child.schema, &child.relname, new_name)?;
        sync_catalog_column(conn, child.child_oid, child_attnum, &column, new_name)?;
    }

    rename_catalog_column(conn, parent_oid, parent_attnum, new_name)?;
    if old_name.eq_ignore_ascii_case(&partition_key) {
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_relation_ext \
                 SET partition_key = '{}', updated_at = CURRENT_TIMESTAMP \
                 WHERE relid = {parent_oid}",
                sql_string(new_name)
            ),
            [],
        )
        .map_err(|e| format!("update renamed partition key metadata failed: {e}"))?;
    }
    refresh_partition_entrypoint(conn, parent_oid, schema, table)
}

fn alter_partitioned_table_set_column_type(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    column_name: &str,
    parent_attnum: i32,
    operation_sql: &str,
) -> Result<(), String> {
    if column_name.eq_ignore_ascii_case(&partition_key_name(conn, parent_oid)?) {
        return Err("ALTER TABLE SET DATA TYPE does not support a partition key column".into());
    }
    let children = active_partition_children(conn, parent_oid)?;
    if children.is_empty() {
        return Err("partitioned table has no active physical partitions".into());
    }

    let mut parent_column = None;
    for child in &children {
        let child_attnum = column_attnum(conn, child.child_oid, column_name)?.ok_or_else(|| {
            format!(
                "partition column does not exist in catalog: {}.{}.{}",
                child.schema, child.relname, column_name
            )
        })?;
        conn.execute(
            &format!(
                "ALTER TABLE {} {}",
                quote_qualified(&child.schema, &child.relname),
                operation_sql
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB ALTER TABLE SET DATA TYPE on partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;
        let column = duckdb_column(conn, &child.schema, &child.relname, column_name)?;
        sync_catalog_column(conn, child.child_oid, child_attnum, &column, column_name)?;
        if parent_column.is_none() {
            parent_column = Some(column);
        }
    }

    let parent_column = parent_column
        .ok_or_else(|| "partitioned table has no active physical partitions".to_string())?;
    sync_catalog_column(conn, parent_oid, parent_attnum, &parent_column, column_name)?;
    refresh_partition_entrypoint(conn, parent_oid, schema, table)
}

fn ensure_no_dependent_views(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    column_name: &str,
    operation: &str,
) -> Result<(), String> {
    if dependent_relation_oids(conn, rel_oid)?.is_empty() {
        return Ok(());
    }
    Err(format!(
        "cannot {operation} for column with dependent views: {schema}.{table}.{column_name}"
    ))
}

fn partition_key_name(conn: &Connection, parent_oid: i64) -> Result<String, String> {
    conn.query_row(
        &format!(
            "SELECT partition_key FROM rsduck_catalog.rs_relation_ext WHERE relid = {parent_oid}"
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read partition key failed: {e}"))
}

fn duckdb_column(
    conn: &Connection,
    schema: &str,
    table: &str,
    column_name: &str,
) -> Result<CatalogColumn, String> {
    load_duckdb_columns(conn, schema, table)?
        .into_iter()
        .find(|column| column.name.eq_ignore_ascii_case(column_name))
        .ok_or_else(|| format!("DuckDB did not expose column: {schema}.{table}.{column_name}"))
}

fn sync_catalog_column(
    conn: &Connection,
    rel_oid: i64,
    attnum: i32,
    column: &CatalogColumn,
    name: &str,
) -> Result<(), String> {
    let type_id = ensure_type_id_for_duckdb_type(conn, &column.duckdb_type)?;
    let type_modifier = type_modifier_for_duckdb_type(&column.duckdb_type);
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_column \
             SET attname = '{}', atttypid = {type_id}, atttypmod = {type_modifier}, \
                 attnotnull = {}, atthasdef = {} \
             WHERE attrelid = {rel_oid} AND attnum = {attnum}",
            sql_string(name),
            sql_bool(column.not_null),
            sql_bool(column.default_expr.is_some())
        ),
        [],
    )
    .map_err(|e| format!("update altered column metadata failed: {e}"))?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_column_default \
             WHERE adrelid = {rel_oid} AND adnum = {attnum}"
        ),
        [],
    )
    .map_err(|e| format!("delete altered column default failed: {e}"))?;
    if let Some(default_expr) = &column.default_expr {
        let default_oid = allocate_oid(conn)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_column_default(oid, adrelid, adnum, adbin) \
                 VALUES ({default_oid}, {rel_oid}, {attnum}, '{}')",
                sql_string(default_expr)
            ),
            [],
        )
        .map_err(|e| format!("write altered column default failed: {e}"))?;
    }
    Ok(())
}

fn rename_catalog_column(
    conn: &Connection,
    rel_oid: i64,
    attnum: i32,
    new_name: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_column SET attname = '{}' \
             WHERE attrelid = {rel_oid} AND attnum = {attnum}",
            sql_string(new_name)
        ),
        [],
    )
    .map_err(|e| format!("update renamed column metadata failed: {e}"))?;
    Ok(())
}

pub(in crate::catalog) fn alter_partitioned_table_add_column(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    column_def_sql: &str,
) -> Result<(), String> {
    let children = active_partition_children(conn, parent_oid)?;
    if children.is_empty() {
        return Err("partitioned table has no active physical partitions".into());
    }

    let new_attnum = next_attribute_num(conn, parent_oid)?;
    let mut parent_column: Option<CatalogColumn> = None;
    for child in &children {
        conn.execute(
            &format!(
                "ALTER TABLE {} ADD COLUMN {column_def_sql}",
                quote_qualified(&child.schema, &child.relname)
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB ALTER TABLE ADD COLUMN on partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;

        let physical_columns = load_duckdb_columns(conn, &child.schema, &child.relname)?;
        let column = physical_columns
            .last()
            .ok_or_else(|| {
                format!(
                    "DuckDB partition has no columns: {}.{}",
                    child.schema, child.relname
                )
            })?
            .clone();
        let mut column = column;
        column.attnum = new_attnum;
        insert_attribute_row(conn, child.child_oid, &column)?;
        set_relnatts_to_active_attribute_count(conn, child.child_oid)?;

        if parent_column.is_none() {
            parent_column = Some(column);
        }
    }

    let parent_column = parent_column
        .ok_or_else(|| "partitioned table has no active physical partitions".to_string())?;
    insert_attribute_row(conn, parent_oid, &parent_column)?;
    set_relnatts_to_active_attribute_count(conn, parent_oid)?;
    refresh_partition_entrypoint(conn, parent_oid, schema, table)
}

pub(in crate::catalog) fn alter_table_drop_columns(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    column_names: &[sqlparser::ast::Ident],
    if_exists: bool,
) -> Result<(), String> {
    if column_names.is_empty() {
        return Err("ALTER TABLE DROP COLUMN requires at least one column".into());
    }
    let relkind = relation_kind(conn, rel_oid)?;
    let columns = drop_column_targets(conn, rel_oid, schema, table, column_names, if_exists)?;
    if columns.is_empty() {
        return Ok(());
    }

    if relkind == "p" {
        return alter_partitioned_table_drop_columns(conn, rel_oid, schema, table, &columns);
    }
    if relkind != "r" {
        return Err(format!(
            "ALTER TABLE DROP COLUMN only supports ordinary or partitioned tables, got relkind={relkind}"
        ));
    }

    for (column_name, attnum) in columns {
        ensure_column_can_drop(conn, rel_oid, attnum, schema, table, &column_name)?;
        conn.execute(
            &format!(
                "ALTER TABLE {} DROP COLUMN {}",
                quote_qualified(schema, table),
                quote_ident(&column_name)
            ),
            [],
        )
        .map_err(|e| format!("execute DuckDB ALTER TABLE DROP COLUMN failed: {e}"))?;
        mark_column_dropped(conn, rel_oid, attnum)?;
    }
    set_relnatts_to_active_attribute_count(conn, rel_oid)
}

pub(in crate::catalog) fn alter_partitioned_table_drop_columns(
    conn: &Connection,
    parent_oid: i64,
    schema: &str,
    table: &str,
    columns: &[(String, i32)],
) -> Result<(), String> {
    let partition_key: String = conn
        .query_row(
            &format!(
                "SELECT partition_key FROM rsduck_catalog.rs_relation_ext WHERE relid = {parent_oid}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read partition key failed: {e}"))?;
    for (column_name, attnum) in columns {
        if column_name.eq_ignore_ascii_case(&partition_key) {
            return Err(format!(
                "cannot drop partition key column: {schema}.{table}.{column_name}"
            ));
        }
        ensure_column_can_drop(conn, parent_oid, *attnum, schema, table, column_name)?;
    }

    let children = active_partition_children(conn, parent_oid)?;
    if children.is_empty() {
        return Err("partitioned table has no active physical partitions".into());
    }

    for child in &children {
        for (column_name, _) in columns {
            let child_attnum =
                column_attnum(conn, child.child_oid, column_name)?.ok_or_else(|| {
                    format!(
                        "partition column does not exist in catalog: {}.{}.{}",
                        child.schema, child.relname, column_name
                    )
                })?;
            conn.execute(
                &format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    quote_qualified(&child.schema, &child.relname),
                    quote_ident(column_name)
                ),
                [],
            )
            .map_err(|e| {
                format!(
                    "execute DuckDB ALTER TABLE DROP COLUMN on partition {}.{} failed: {e}",
                    child.schema, child.relname
                )
            })?;
            mark_column_dropped(conn, child.child_oid, child_attnum)?;
        }
        set_relnatts_to_active_attribute_count(conn, child.child_oid)?;
    }

    for (_, attnum) in columns {
        mark_column_dropped(conn, parent_oid, *attnum)?;
    }
    set_relnatts_to_active_attribute_count(conn, parent_oid)?;
    refresh_partition_entrypoint(conn, parent_oid, schema, table)
}

pub(in crate::catalog) fn drop_column_targets(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    column_names: &[sqlparser::ast::Ident],
    if_exists: bool,
) -> Result<Vec<(String, i32)>, String> {
    let mut columns = Vec::new();
    for column_name in column_names {
        let name = column_name.value.clone();
        match column_attnum(conn, rel_oid, &name)? {
            Some(attnum) => columns.push((name, attnum)),
            None if if_exists => {}
            None => {
                return Err(format!(
                    "column does not exist in catalog: {schema}.{table}.{name}"
                ))
            }
        }
    }
    Ok(columns)
}

pub(in crate::catalog) fn ensure_column_can_drop(
    conn: &Connection,
    rel_oid: i64,
    attnum: i32,
    schema: &str,
    table: &str,
    column_name: &str,
) -> Result<(), String> {
    if column_attnum_list_contains(
        conn,
        "rsduck_catalog.rs_constraint",
        "conrelid",
        "conkey",
        rel_oid,
        attnum,
    )? {
        return Err(format!(
            "cannot drop column with constraint dependency: {schema}.{table}.{column_name}"
        ));
    }
    if column_attnum_list_contains(
        conn,
        "rsduck_catalog.rs_constraint",
        "confrelid",
        "confkey",
        rel_oid,
        attnum,
    )? {
        return Err(format!(
            "cannot drop referenced column with foreign key dependency: {schema}.{table}.{column_name}"
        ));
    }
    if column_attnum_list_contains(
        conn,
        "rsduck_catalog.rs_index",
        "indrelid",
        "indkey",
        rel_oid,
        attnum,
    )? {
        return Err(format!(
            "cannot drop column with index dependency: {schema}.{table}.{column_name}"
        ));
    }
    Ok(())
}

pub(in crate::catalog) fn column_attnum_list_contains(
    conn: &Connection,
    table_name: &str,
    relid_column: &str,
    list_column: &str,
    rel_oid: i64,
    attnum: i32,
) -> Result<bool, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {list_column} FROM {table_name} WHERE {relid_column} = {rel_oid}"
        ))
        .map_err(|e| format!("prepare column dependency lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query column dependency lookup failed: {e}"))?;
    let attnum = attnum.to_string();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read column dependency lookup failed: {e}"))?
    {
        let value: String = row
            .get(0)
            .map_err(|e| format!("read column dependency list failed: {e}"))?;
        if value
            .split(',')
            .map(str::trim)
            .any(|part| part == attnum.as_str())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(in crate::catalog) fn mark_column_dropped(
    conn: &Connection,
    rel_oid: i64,
    attnum: i32,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_column \
             SET attisdropped = TRUE, atthasdef = FALSE \
             WHERE attrelid = {rel_oid} AND attnum = {attnum}"
        ),
        [],
    )
    .map_err(|e| format!("mark column dropped failed: {e}"))?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_column_default WHERE adrelid = {rel_oid} AND adnum = {attnum}"
        ),
        [],
    )
    .map_err(|e| format!("delete dropped column default failed: {e}"))?;
    Ok(())
}

pub(in crate::catalog) fn next_attribute_num(
    conn: &Connection,
    rel_oid: i64,
) -> Result<i32, String> {
    let max_attnum: Option<i32> = conn
        .query_row(
            &format!("SELECT MAX(attnum) FROM rsduck_catalog.rs_column WHERE attrelid = {rel_oid}"),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read max attribute number failed: {e}"))?;
    Ok(max_attnum.unwrap_or(0) + 1)
}

pub(in crate::catalog) fn set_relnatts_to_active_attribute_count(
    conn: &Connection,
    rel_oid: i64,
) -> Result<(), String> {
    let active_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_column \
                 WHERE attrelid = {rel_oid} AND attisdropped = FALSE"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("count active attributes failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation SET relnatts = {active_count} WHERE oid = {rel_oid}"
        ),
        [],
    )
    .map_err(|e| format!("update active relnatts failed: {e}"))?;
    Ok(())
}

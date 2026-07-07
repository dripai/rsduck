fn insert_create_table_constraints(
    conn: &Connection,
    rel_oid: i64,
    schema: &str,
    table: &str,
    columns: &[CatalogColumn],
    create_table: &CreateTable,
) -> Result<(), String> {
    let namespace_oid = namespace_oid(conn, schema)?;
    for (idx, column) in create_table.columns.iter().enumerate() {
        for option in &column.options {
            match &option.option {
                ColumnOption::PrimaryKey(_) => {
                    let con_oid = allocate_oid(conn)?;
                    let conname = option
                        .name
                        .as_ref()
                        .map(|name| name.value.clone())
                        .unwrap_or_else(|| format!("{table}_pkey"));
                    let attnum = columns.get(idx).map(|c| c.attnum).ok_or_else(|| {
                        format!("primary key column is not materialized: {}", column.name)
                    })?;
                    insert_constraint(
                        conn,
                        con_oid,
                        &conname,
                        namespace_oid,
                        "p",
                        rel_oid,
                        &attnum.to_string(),
                        0,
                        "",
                        "",
                    )?;
                }
                ColumnOption::ForeignKey(fk) => {
                    let con_oid = allocate_oid(conn)?;
                    let conname = option
                        .name
                        .as_ref()
                        .map(|name| name.value.clone())
                        .unwrap_or_else(|| format!("{}_{}_fkey", table, column.name.value));
                    let attnum = columns.get(idx).map(|c| c.attnum).ok_or_else(|| {
                        format!("foreign key column is not materialized: {}", column.name)
                    })?;
                    let (foreign_relid, confkey) = foreign_key_reference(conn, schema, table, fk)?;
                    insert_constraint(
                        conn,
                        con_oid,
                        &conname,
                        namespace_oid,
                        "f",
                        rel_oid,
                        &attnum.to_string(),
                        foreign_relid,
                        &confkey,
                        &fk.to_string(),
                    )?;
                }
                _ => {}
            }
        }
    }

    for constraint in &create_table.constraints {
        match constraint {
            TableConstraint::PrimaryKey(pk) => {
                let con_oid = allocate_oid(conn)?;
                let conname = pk
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_pkey"));
                let conkey = index_columns_to_attnums(&pk.columns, columns)?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "p",
                    rel_oid,
                    &conkey,
                    0,
                    "",
                    "",
                )?;
            }
            TableConstraint::Unique(unique) => {
                let con_oid = allocate_oid(conn)?;
                let conname = unique
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_key"));
                let conkey = index_columns_to_attnums(&unique.columns, columns)?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "u",
                    rel_oid,
                    &conkey,
                    0,
                    "",
                    "",
                )?;
            }
            TableConstraint::Check(check) => {
                let con_oid = allocate_oid(conn)?;
                let conname = check
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_check"));
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "c",
                    rel_oid,
                    "",
                    0,
                    "",
                    &check.expr.to_string(),
                )?;
            }
            TableConstraint::ForeignKey(fk) => {
                let con_oid = allocate_oid(conn)?;
                let conname = fk
                    .name
                    .as_ref()
                    .map(|name| name.value.clone())
                    .unwrap_or_else(|| format!("{table}_fkey"));
                let conkey = ident_columns_to_attnums(&fk.columns, columns)?;
                let (foreign_relid, confkey) = foreign_key_reference(conn, schema, table, fk)?;
                insert_constraint(
                    conn,
                    con_oid,
                    &conname,
                    namespace_oid,
                    "f",
                    rel_oid,
                    &conkey,
                    foreign_relid,
                    &confkey,
                    &fk.to_string(),
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn insert_constraint(
    conn: &Connection,
    oid: i64,
    conname: &str,
    namespace_oid: i64,
    contype: &str,
    rel_oid: i64,
    conkey: &str,
    confrelid: i64,
    confkey: &str,
    conbin: &str,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_constraint(oid, conname, connamespace, contype, conrelid, \
             conindid, conkey, confrelid, confkey, convalidated, conbin) \
             VALUES ({oid}, '{}', {namespace_oid}, '{}', {rel_oid}, 0, '{}', {confrelid}, '{}', TRUE, '{}')",
            sql_string(conname),
            sql_string(contype),
            sql_string(conkey),
            sql_string(confkey),
            sql_string(conbin)
        ),
        [],
    )
    .map_err(|e| format!("write pg_constraint failed: {e}"))?;
    insert_constraint_dependencies(conn, oid, rel_oid, conkey, confrelid, confkey)?;
    Ok(())
}

fn foreign_key_reference(
    conn: &Connection,
    local_schema: &str,
    table: &str,
    fk: &ForeignKeyConstraint,
) -> Result<(i64, String), String> {
    if fk.index_name.is_some()
        || fk.on_delete.is_some()
        || fk.on_update.is_some()
        || fk.match_kind.is_some()
        || fk.characteristics.is_some()
    {
        return Err("foreign key options are not supported by rsduck catalog".into());
    }
    if fk.referred_columns.is_empty() {
        return Err("foreign key must specify referenced columns".into());
    }
    let (foreign_schema, foreign_table) =
        relation_name_with_default(&fk.foreign_table, local_schema)?;
    let foreign_meta =
        find_relation_meta(conn, &foreign_schema, &foreign_table)?.ok_or_else(|| {
            format!("foreign key referenced table does not exist: {foreign_schema}.{foreign_table}")
        })?;
    if foreign_meta.relkind != "r" {
        return Err(format!(
            "foreign key referenced table must be ordinary table: {foreign_schema}.{foreign_table}"
        ));
    }
    let foreign_columns = catalog_columns(conn, foreign_meta.oid)?;
    let confkey = ident_columns_to_attnums(&fk.referred_columns, &foreign_columns)?;
    if !fk.columns.is_empty() && fk.columns.len() != fk.referred_columns.len() {
        return Err(format!(
            "foreign key column count mismatch on {table}: local={}, referenced={}",
            fk.columns.len(),
            fk.referred_columns.len()
        ));
    }
    Ok((foreign_meta.oid, confkey))
}

fn index_columns_to_attnums(
    index_columns: &[sqlparser::ast::IndexColumn],
    columns: &[CatalogColumn],
) -> Result<String, String> {
    let mut attnums = Vec::with_capacity(index_columns.len());
    for index_column in index_columns {
        let column_name = index_column.column.expr.to_string();
        let attnum = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&column_name))
            .map(|column| column.attnum)
            .ok_or_else(|| format!("constraint references unknown column: {column_name}"))?;
        attnums.push(attnum.to_string());
    }
    Ok(attnums.join(","))
}

fn ident_columns_to_attnums(
    idents: &[sqlparser::ast::Ident],
    columns: &[CatalogColumn],
) -> Result<String, String> {
    let mut attnums = Vec::with_capacity(idents.len());
    for ident in idents {
        let attnum = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&ident.value))
            .map(|column| column.attnum)
            .ok_or_else(|| format!("constraint references unknown column: {}", ident.value))?;
        attnums.push(attnum.to_string());
    }
    Ok(attnums.join(","))
}

fn insert_constraint_dependencies(
    conn: &Connection,
    constraint_oid: i64,
    rel_oid: i64,
    conkey: &str,
    confrelid: i64,
    confkey: &str,
) -> Result<(), String> {
    insert_depend_if_missing(
        conn,
        PG_CONSTRAINT_CLASSOID,
        constraint_oid,
        0,
        PG_CLASS_CLASSOID,
        rel_oid,
        0,
        "n",
    )?;
    for attnum in parse_attnums(conkey) {
        insert_depend_if_missing(
            conn,
            PG_CONSTRAINT_CLASSOID,
            constraint_oid,
            0,
            PG_CLASS_CLASSOID,
            rel_oid,
            attnum,
            "n",
        )?;
    }
    if confrelid > 0 {
        insert_depend_if_missing(
            conn,
            PG_CONSTRAINT_CLASSOID,
            constraint_oid,
            0,
            PG_CLASS_CLASSOID,
            confrelid,
            0,
            "n",
        )?;
        for attnum in parse_attnums(confkey) {
            insert_depend_if_missing(
                conn,
                PG_CONSTRAINT_CLASSOID,
                constraint_oid,
                0,
                PG_CLASS_CLASSOID,
                confrelid,
                attnum,
                "n",
            )?;
        }
    }
    Ok(())
}

fn parse_attnums(value: &str) -> Vec<i32> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i32>().ok())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn insert_depend_if_missing(
    conn: &Connection,
    classid: i64,
    objid: i64,
    objsubid: i32,
    refclassid: i64,
    refobjid: i64,
    refobjsubid: i32,
    deptype: &str,
) -> Result<(), String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.pg_depend \
                 WHERE classid = {classid} AND objid = {objid} AND objsubid = {objsubid} \
                   AND refclassid = {refclassid} AND refobjid = {refobjid} \
                   AND refobjsubid = {refobjsubid}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check dependency failed: {e}"))?;
    if count > 0 {
        return Ok(());
    }
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.pg_depend(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
             VALUES ({classid}, {objid}, {objsubid}, {refclassid}, {refobjid}, {refobjsubid}, '{}')",
            sql_string(deptype)
        ),
        [],
    )
    .map_err(|e| format!("write dependency failed: {e}"))?;
    Ok(())
}

fn simple_index_column_names(
    index_columns: &[sqlparser::ast::IndexColumn],
) -> Result<Vec<String>, String> {
    let mut names = Vec::with_capacity(index_columns.len());
    for index_column in index_columns {
        if index_column.operator_class.is_some() || index_column.column.with_fill.is_some() {
            return Err("index column options are not supported by rsduck catalog".into());
        }
        match &index_column.column.expr {
            Expr::Identifier(ident) => names.push(ident.value.clone()),
            _ => return Err("expression index is not supported by rsduck catalog".into()),
        }
    }
    if names.is_empty() {
        return Err("CREATE INDEX requires at least one column".into());
    }
    Ok(names)
}

fn load_duckdb_columns(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Vec<CatalogColumn>, String> {
    let sql = format!(
        "SELECT column_name, data_type, is_nullable, column_default, column_index \
         FROM duckdb_columns() \
         WHERE schema_name = '{}' AND table_name = '{}' AND internal = FALSE \
         ORDER BY column_index",
        sql_string(schema),
        sql_string(relation)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare duckdb_columns query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query duckdb_columns failed: {e}"))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read duckdb_columns failed: {e}"))?
    {
        let name: String = row
            .get(0)
            .map_err(|e| format!("read column_name failed: {e}"))?;
        let duckdb_type: String = row
            .get(1)
            .map_err(|e| format!("read data_type failed: {e}"))?;
        let is_nullable: bool = row
            .get(2)
            .map_err(|e| format!("read is_nullable failed: {e}"))?;
        let default_expr: Option<String> = row
            .get(3)
            .map_err(|e| format!("read column_default failed: {e}"))?;
        let column_index: i32 = row
            .get(4)
            .map_err(|e| format!("read column_index failed: {e}"))?;
        columns.push(CatalogColumn {
            name,
            pg_type_oid: pg_type_oid_for_duckdb_type(&duckdb_type)?,
            attnum: column_index,
            not_null: !is_nullable,
            default_expr,
        });
    }
    if columns.is_empty() {
        return Err(format!(
            "DuckDB relation has no columns: {schema}.{relation}"
        ));
    }
    Ok(columns)
}


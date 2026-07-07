fn create_schema(
    conn: &Connection,
    schema_name: &SchemaName,
    if_not_exists: bool,
    owner_user_id: i64,
) -> Result<usize, String> {
    let schema = schema_name_value(schema_name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if namespace_exists(conn, &schema)? {
            if if_not_exists {
                return Ok(0);
            }
            return Err(format!("schema already exists: {schema}"));
        }

        let ns_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_schema", ns_oid, &schema)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.pg_namespace(oid, nspname, nspowner, nspacl) \
                 VALUES ({ns_oid}, '{}', {owner_user_id}, '')",
                sql_string(&schema)
            ),
            [],
        )
        .map_err(|e| format!("write pg_namespace failed: {e}"))?;
        conn.execute(&format!("CREATE SCHEMA {}", quote_ident(&schema)), [])
            .map_err(|e| format!("execute DuckDB CREATE SCHEMA failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}


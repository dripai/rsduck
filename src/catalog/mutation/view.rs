fn create_view_relation(
    conn: &Connection,
    create_view: &CreateView,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_view.or_replace {
        return Err("CREATE OR REPLACE VIEW is not supported by catalog mutation yet".into());
    }
    if create_view.temporary {
        return Err("temporary view is not supported by rsduck catalog".into());
    }

    let (schema, view) = relation_name(&create_view.name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if relation_exists(conn, &schema, &view)? {
            if create_view.if_not_exists {
                return Ok(0);
            }
            return Err(format!("relation already exists: {schema}.{view}"));
        }

        ensure_user_schema_exists(conn, &schema)?;
        let rel_oid = allocate_oid(conn)?;
        let type_oid = allocate_oid(conn)?;
        let journal_id = insert_journal(conn, "create_view", rel_oid, sql)?;

        conn.execute(sql, [])
            .map_err(|e| format!("execute DuckDB CREATE VIEW failed: {e}"))?;

        let columns = load_duckdb_columns(conn, &schema, &view)?;
        insert_relation_rows(
            conn,
            rel_oid,
            type_oid,
            &schema,
            &view,
            "v",
            "generated_view",
            "user",
            &create_view.query.to_string(),
            &columns,
            owner_user_id,
        )?;
        insert_view_dependencies(conn, rel_oid, sql)?;
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

fn insert_view_dependencies(conn: &Connection, view_oid: i64, sql: &str) -> Result<(), String> {
    for (schema, relation) in extract_read_relations(&normalize_for_guard(sql)) {
        let ref_oid = relation_oid(conn, &schema, &relation)?;
        if ref_oid == view_oid {
            continue;
        }
        insert_depend_if_missing(
            conn,
            PG_CLASS_CLASSOID,
            view_oid,
            0,
            PG_CLASS_CLASSOID,
            ref_oid,
            0,
            "n",
        )?;
    }
    Ok(())
}


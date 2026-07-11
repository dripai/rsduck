use super::*;

pub(in crate::catalog) fn create_view_relation(
    conn: &Connection,
    create_view: &CreateView,
    sql: &str,
    owner_user_id: i64,
) -> Result<usize, String> {
    if create_view.temporary {
        return Err("temporary view is not supported by rsduck catalog".into());
    }

    let (schema, view) = relation_name(&create_view.name)?;
    reject_reserved_schema(&schema)?;

    run_catalog_tx(conn, || {
        if let Some(meta) = find_relation_meta(conn, &schema, &view)? {
            if create_view.or_replace {
                return replace_view_relation(conn, &meta, &schema, &view, create_view, sql);
            }
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

fn replace_view_relation(
    conn: &Connection,
    meta: &RelationMeta,
    schema: &str,
    view: &str,
    create_view: &CreateView,
    sql: &str,
) -> Result<usize, String> {
    if meta.relkind != "v" {
        return Err(format!(
            "CREATE OR REPLACE VIEW cannot replace relation with relkind={}",
            meta.relkind
        ));
    }

    let journal_id = insert_journal(conn, "replace_view", meta.oid, sql)?;
    conn.execute(sql, [])
        .map_err(|e| format!("execute DuckDB CREATE OR REPLACE VIEW failed: {e}"))?;

    let columns = load_duckdb_columns(conn, schema, view)?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_column_default WHERE adrelid = {}",
            meta.oid
        ),
        [],
    )
    .map_err(|e| format!("delete replaced view column defaults failed: {e}"))?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_column WHERE attrelid = {}",
            meta.oid
        ),
        [],
    )
    .map_err(|e| format!("delete replaced view columns failed: {e}"))?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_comment \
             WHERE objoid = {} AND classoid = {} AND objsubid > 0",
            meta.oid, OBJECT_RELATION_KIND
        ),
        [],
    )
    .map_err(|e| format!("delete replaced view column comments failed: {e}"))?;
    conn.execute(
        &format!(
            "DELETE FROM rsduck_catalog.rs_dependency \
             WHERE classid = {} AND objid = {}",
            OBJECT_RELATION_KIND, meta.oid
        ),
        [],
    )
    .map_err(|e| format!("delete replaced view dependencies failed: {e}"))?;

    for column in &columns {
        insert_attribute_row(conn, meta.oid, column)?;
    }
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation \
             SET relnatts = {}, status = 'active', error_message = '' \
             WHERE oid = {}",
            columns.len(),
            meta.oid
        ),
        [],
    )
    .map_err(|e| format!("update replaced view relation metadata failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_relation_ext \
             SET generated_sql = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE relid = {}",
            sql_string(&create_view.query.to_string()),
            meta.oid
        ),
        [],
    )
    .map_err(|e| format!("update replaced view definition failed: {e}"))?;
    insert_view_dependencies(conn, meta.oid, sql)?;
    finish_journal(conn, journal_id)?;
    Ok(0)
}

pub(in crate::catalog) fn insert_view_dependencies(
    conn: &Connection,
    view_oid: i64,
    sql: &str,
) -> Result<(), String> {
    for (schema, relation) in extract_read_relations(&normalize_for_guard(sql)) {
        let ref_oid = relation_oid(conn, &schema, &relation)?;
        if ref_oid == view_oid {
            continue;
        }
        insert_depend_if_missing(
            conn,
            OBJECT_RELATION_KIND,
            view_oid,
            0,
            OBJECT_RELATION_KIND,
            ref_oid,
            0,
            "n",
        )?;
    }
    Ok(())
}

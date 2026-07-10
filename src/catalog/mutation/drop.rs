use super::*;

pub(in crate::catalog) fn drop_objects(
    conn: &Connection,
    object_type: ObjectType,
    if_exists: bool,
    names: &[ObjectName],
    cascade: bool,
    table: Option<&ObjectName>,
    sql: &str,
) -> Result<usize, String> {
    if names.is_empty() {
        return Err("DROP requires at least one object name".into());
    }
    if object_type == ObjectType::User {
        return drop_user_accounts(conn, if_exists, names, sql);
    }
    if object_type == ObjectType::Role {
        return drop_role_accounts(conn, if_exists, names, cascade, sql);
    }

    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in names {
            let (schema, relname) = match (object_type, table) {
                (ObjectType::Index, Some(table_name)) => {
                    let (table_schema, _) = relation_name(table_name)?;
                    relation_name_with_default(name, &table_schema)?
                }
                _ => relation_name(name)?,
            };
            reject_reserved_schema(&schema)?;
            let Some(meta) = find_relation_meta(conn, &schema, &relname)? else {
                if if_exists {
                    continue;
                }
                return Err(format!("relation does not exist: {schema}.{relname}"));
            };
            ensure_drop_type(object_type, &meta)?;
            if meta.relispartition {
                return Err("cannot directly drop managed physical partition".into());
            }

            let journal_id = insert_journal(conn, "drop_relation", meta.oid, sql)?;
            drop_relation_dependencies(conn, &meta, cascade)?;
            if meta.relkind == "p" {
                drop_partitioned_relation(conn, &meta, &schema, &relname)?;
            } else if meta.relkind == "i" && partitioned_index_parent(conn, meta.oid)?.is_some() {
                drop_partitioned_index(conn, &meta)?;
            } else {
                execute_physical_drop(conn, object_type, &schema, &relname, cascade)?;
                delete_relation_catalog(conn, &meta)?;
            }
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

pub(in crate::catalog) fn drop_role_accounts(
    conn: &Connection,
    if_exists: bool,
    names: &[ObjectName],
    cascade: bool,
    sql: &str,
) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in names {
            let role_name = single_name_part(name)?;
            let Some(role_id) = role_id_by_name_opt(conn, &role_name)? else {
                if if_exists {
                    continue;
                }
                return Err(format!("role does not exist: {role_name}"));
            };
            if builtin_role(conn, role_id)? {
                return Err(format!("builtin role cannot be dropped: {role_name}"));
            }
            if !cascade && role_has_dependents(conn, role_id)? {
                return Err(format!(
                    "cannot drop role with granted users or privileges; revoke grants first: {role_name}"
                ));
            }
            let journal_id = insert_journal(conn, "drop_role", role_id, sql)?;
            for statement in [
                format!("DELETE FROM rsduck_catalog.rs_user_role WHERE role_id = {role_id}"),
                format!(
                    "DELETE FROM rsduck_catalog.rs_privilege \
                     WHERE principal_type = 'role' AND principal_id = {role_id}"
                ),
                format!("DELETE FROM rsduck_catalog.rs_role WHERE role_id = {role_id}"),
            ] {
                conn.execute(&statement, [])
                    .map_err(|e| format!("drop role catalog rows failed: {e}"))?;
            }
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

pub(in crate::catalog) fn drop_user_accounts(
    conn: &Connection,
    if_exists: bool,
    names: &[ObjectName],
    sql: &str,
) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let mut affected = 0;
        for name in names {
            let username = single_name_part(name)?;
            if username.eq_ignore_ascii_case("admin") {
                return Err("default admin user cannot be dropped".into());
            }
            let Some(user_id) = user_id_by_name_opt(conn, &username)? else {
                if if_exists {
                    continue;
                }
                return Err(format!("user does not exist: {username}"));
            };
            let journal_id = insert_journal(conn, "drop_user", user_id, sql)?;
            for statement in [
                format!("DELETE FROM rsduck_catalog.rs_user_role WHERE user_id = {user_id}"),
                format!(
                    "DELETE FROM rsduck_catalog.rs_privilege \
                     WHERE principal_type = 'user' AND principal_id = {user_id}"
                ),
                format!("DELETE FROM rsduck_catalog.rs_user WHERE user_id = {user_id}"),
            ] {
                conn.execute(&statement, [])
                    .map_err(|e| format!("drop user catalog rows failed: {e}"))?;
            }
            finish_journal(conn, journal_id)?;
            affected += 1;
        }
        Ok(affected)
    })
}

pub(in crate::catalog) fn drop_partitioned_relation(
    conn: &Connection,
    meta: &RelationMeta,
    schema: &str,
    relname: &str,
) -> Result<(), String> {
    let children = partition_child_metas(conn, meta.oid)?;
    conn.execute(
        &format!("DROP VIEW {}", quote_qualified(schema, relname)),
        [],
    )
    .map_err(|e| format!("execute DuckDB DROP partition entrypoint failed: {e}"))?;
    for child in children {
        conn.execute(
            &format!(
                "DROP TABLE {}",
                quote_qualified(&child.schema, &child.relname)
            ),
            [],
        )
        .map_err(|e| {
            format!(
                "execute DuckDB DROP physical partition {}.{} failed: {e}",
                child.schema, child.relname
            )
        })?;
        delete_relation_catalog(conn, &child.meta)?;
    }
    delete_relation_catalog(conn, meta)
}

pub(in crate::catalog) fn drop_partitioned_index(
    conn: &Connection,
    meta: &RelationMeta,
) -> Result<(), String> {
    if let Some(parent_oid) = partitioned_index_parent(conn, meta.oid)? {
        for spec in partition_index_specs(conn, parent_oid)?
            .into_iter()
            .filter(|spec| spec.index_oid == meta.oid)
        {
            for partition in active_partition_children(conn, parent_oid)? {
                let child_index = partition_index_name(&partition.relname, &spec.index_name);
                if duckdb_index_exists(conn, "rsduck_internal", &child_index)? {
                    conn.execute(
                        &format!("DROP INDEX {}", quote_ident(&child_index)),
                        [],
                    )
                    .map_err(|e| {
                        format!(
                            "execute DuckDB DROP partition index rsduck_internal.{child_index} failed: {e}"
                        )
                    })?;
                }
            }
        }
    }
    delete_relation_catalog(conn, meta)
}

#[derive(Debug)]
pub(in crate::catalog) struct PartitionChildMeta {
    pub(in crate::catalog) meta: RelationMeta,
    pub(in crate::catalog) schema: String,
    pub(in crate::catalog) relname: String,
}

pub(in crate::catalog) fn partition_child_metas(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<PartitionChildMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.oid, c.reltype, c.relkind, c.relispartition, n.nspname, c.relname \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.rs_relation c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
             ORDER BY p.is_null_partition, p.partition_value"
        ))
        .map_err(|e| format!("prepare partition child lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition child lookup failed: {e}"))?;
    let mut children = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition child lookup failed: {e}"))?
    {
        children.push(PartitionChildMeta {
            meta: RelationMeta {
                oid: row
                    .get(0)
                    .map_err(|e| format!("read partition child oid failed: {e}"))?,
                reltype: row
                    .get(1)
                    .map_err(|e| format!("read partition child reltype failed: {e}"))?,
                relkind: row
                    .get(2)
                    .map_err(|e| format!("read partition child relkind failed: {e}"))?,
                relispartition: row
                    .get(3)
                    .map_err(|e| format!("read partition child flag failed: {e}"))?,
            },
            schema: row
                .get(4)
                .map_err(|e| format!("read partition child schema failed: {e}"))?,
            relname: row
                .get(5)
                .map_err(|e| format!("read partition child name failed: {e}"))?,
        });
    }
    Ok(children)
}

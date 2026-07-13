use super::*;

pub(super) fn namespace_exists(conn: &Connection, schema: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_schema WHERE lower(nspname) = lower('{}')",
                sql_string(schema)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check namespace failed: {e}"))?;
    Ok(count > 0)
}

pub(super) fn ensure_user_schema_exists(conn: &Connection, schema: &str) -> Result<(), String> {
    if namespace_exists(conn, schema)? {
        Ok(())
    } else {
        Err(format!("schema does not exist: {schema}"))
    }
}

pub(super) fn validate_username(username: &str) -> Result<(), String> {
    if username.is_empty() {
        return Err("username cannot be empty".into());
    }
    if !username
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "username contains unsupported characters: {username}"
        ));
    }
    Ok(())
}

pub(super) fn validate_role_name(role_name: &str) -> Result<(), String> {
    if role_name.is_empty() {
        return Err("role name cannot be empty".into());
    }
    if !role_name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "role name contains unsupported characters: {role_name}"
        ));
    }
    Ok(())
}

pub(super) fn user_exists(conn: &Connection, username: &str) -> Result<bool, String> {
    Ok(user_id_by_name_opt(conn, username)?.is_some())
}

pub(super) fn user_id_by_name(conn: &Connection, username: &str) -> Result<i64, String> {
    user_id_by_name_opt(conn, username)?.ok_or_else(|| format!("user does not exist: {username}"))
}

pub(super) fn user_id_by_name_opt(
    conn: &Connection,
    username: &str,
) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT user_id FROM rsduck_catalog.rs_user WHERE lower(username) = lower('{}')",
            sql_string(username)
        ))
        .map_err(|e| format!("prepare user lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query user lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read user lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read user id failed: {e}"))
}

pub(super) fn role_id_by_name(conn: &Connection, role_name: &str) -> Result<i64, String> {
    role_id_by_name_opt(conn, role_name)?.ok_or_else(|| format!("role does not exist: {role_name}"))
}

pub(super) fn role_id_by_name_opt(
    conn: &Connection,
    role_name: &str,
) -> Result<Option<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT role_id FROM rsduck_catalog.rs_role WHERE lower(role_name) = lower('{}')",
            sql_string(role_name)
        ))
        .map_err(|e| format!("prepare role lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query role lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read role lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read role id failed: {e}"))
}

pub(super) fn builtin_role(conn: &Connection, role_id: i64) -> Result<bool, String> {
    conn.query_row(
        &format!("SELECT is_builtin FROM rsduck_catalog.rs_role WHERE role_id = {role_id}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read role builtin flag failed: {e}"))
}

pub(super) fn role_has_dependents(conn: &Connection, role_id: i64) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT \
                    (SELECT COUNT(*) FROM rsduck_catalog.rs_user_role WHERE role_id = {role_id}) + \
                    (SELECT COUNT(*) FROM rsduck_catalog.rs_privilege WHERE principal_type = 'role' AND principal_id = {role_id})"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check role dependents failed: {e}"))?;
    Ok(count > 0)
}

pub(super) fn namespace_oid(conn: &Connection, schema: &str) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT oid FROM rsduck_catalog.rs_schema WHERE lower(nspname) = lower('{}')",
            sql_string(schema)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("namespace does not exist in catalog: {schema}: {e}"))
}

pub(super) fn relation_exists(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) \
                 FROM rsduck_catalog.rs_relation c \
                 JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
                 WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}')",
                sql_string(schema),
                sql_string(relation)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check relation failed: {e}"))?;
    Ok(count > 0)
}

pub(super) fn find_relation_meta(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Option<RelationMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.oid, c.reltype, c.relkind, c.relispartition \
             FROM rsduck_catalog.rs_relation c \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}') \
               AND c.status = 'active'",
            sql_string(schema),
            sql_string(relation)
        ))
        .map_err(|e| format!("prepare relation lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query relation lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read relation lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(RelationMeta {
        oid: row
            .get(0)
            .map_err(|e| format!("read relation oid failed: {e}"))?,
        reltype: row
            .get(1)
            .map_err(|e| format!("read relation type oid failed: {e}"))?,
        relkind: row
            .get(2)
            .map_err(|e| format!("read relation kind failed: {e}"))?,
        relispartition: row
            .get(3)
            .map_err(|e| format!("read relation partition flag failed: {e}"))?,
    }))
}

pub(super) fn relation_oid(conn: &Connection, schema: &str, relation: &str) -> Result<i64, String> {
    conn.query_row(
        &format!(
            "SELECT c.oid \
             FROM rsduck_catalog.rs_relation c \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}') \
               AND c.status = 'active'",
            sql_string(schema),
            sql_string(relation)
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("relation does not exist in catalog: {schema}.{relation}: {e}"))
}

pub(super) fn available_relation_oid(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<i64, String> {
    let Some(meta) = find_relation_access_meta(conn, schema, relation)? else {
        return Err(format!(
            "relation does not exist in catalog: {schema}.{relation}"
        ));
    };
    if meta.status == "active" {
        return Ok(meta.oid);
    }
    Err(format!(
        "relation is unavailable: {schema}.{relation}: {}",
        meta.error_message
    ))
}

pub(super) fn find_relation_access_meta(
    conn: &Connection,
    schema: &str,
    relation: &str,
) -> Result<Option<RelationAccessMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.oid, c.status, c.error_message \
             FROM rsduck_catalog.rs_relation c \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE lower(n.nspname) = lower('{}') AND lower(c.relname) = lower('{}')",
            sql_string(schema),
            sql_string(relation)
        ))
        .map_err(|e| format!("prepare relation access lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query relation access lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read relation access lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(RelationAccessMeta {
        oid: row
            .get(0)
            .map_err(|e| format!("read relation access oid failed: {e}"))?,
        status: row
            .get(1)
            .map_err(|e| format!("read relation access status failed: {e}"))?,
        error_message: row
            .get(2)
            .map_err(|e| format!("read relation access error failed: {e}"))?,
    }))
}

pub(super) fn column_exists(
    conn: &Connection,
    rel_oid: i64,
    column_name: &str,
) -> Result<bool, String> {
    Ok(column_attnum(conn, rel_oid, column_name)?.is_some())
}

pub(super) fn column_attnum(
    conn: &Connection,
    rel_oid: i64,
    column_name: &str,
) -> Result<Option<i32>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT attnum FROM rsduck_catalog.rs_column \
             WHERE attrelid = {rel_oid} AND lower(attname) = lower('{}') AND attisdropped = FALSE",
            sql_string(column_name)
        ))
        .map_err(|e| format!("prepare column lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query column lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read column lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read column attnum failed: {e}"))
}

pub(super) fn column_name_by_attnum(
    conn: &Connection,
    rel_oid: i64,
    attnum: i32,
) -> Result<String, String> {
    conn.query_row(
        &format!(
            "SELECT attname FROM rsduck_catalog.rs_column \
             WHERE attrelid = {rel_oid} AND attnum = {attnum} AND attisdropped = FALSE"
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| {
        format!("column attnum does not exist in catalog: rel={rel_oid} attnum={attnum}: {e}")
    })
}

pub(super) fn relation_kind(conn: &Connection, rel_oid: i64) -> Result<String, String> {
    conn.query_row(
        &format!("SELECT relkind FROM rsduck_catalog.rs_relation WHERE oid = {rel_oid}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read relation kind failed: {e}"))
}

pub(super) fn relation_name_by_oid(
    conn: &Connection,
    rel_oid: i64,
) -> Result<(String, String), String> {
    conn.query_row(
        &format!(
            "SELECT n.nspname, c.relname \
             FROM rsduck_catalog.rs_relation c \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE c.oid = {rel_oid}"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(|e| format!("relation oid does not exist in catalog: {rel_oid}: {e}"))
}

pub(super) fn catalog_columns(
    conn: &Connection,
    rel_oid: i64,
) -> Result<Vec<CatalogColumn>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT a.attname, a.atttypid, t.rsduck_physical_type, a.attnum, a.attnotnull, d.adbin \
             FROM rsduck_catalog.rs_column a \
             JOIN rsduck_catalog.rs_type t ON t.oid = a.atttypid \
             LEFT JOIN rsduck_catalog.rs_column_default d \
               ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = {rel_oid} AND a.attisdropped = FALSE \
             ORDER BY a.attnum"
        ))
        .map_err(|e| format!("prepare catalog column query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query catalog columns failed: {e}"))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read catalog columns failed: {e}"))?
    {
        columns.push(CatalogColumn {
            name: row
                .get(0)
                .map_err(|e| format!("read catalog attname failed: {e}"))?,
            type_id: row
                .get(1)
                .map_err(|e| format!("read catalog atttypid failed: {e}"))?,
            duckdb_type: row
                .get(2)
                .map_err(|e| format!("read catalog physical type failed: {e}"))?,
            attnum: row
                .get(3)
                .map_err(|e| format!("read catalog attnum failed: {e}"))?,
            not_null: row
                .get(4)
                .map_err(|e| format!("read catalog attnotnull failed: {e}"))?,
            default_expr: row
                .get(5)
                .map_err(|e| format!("read catalog adbin failed: {e}"))?,
        });
    }
    Ok(columns)
}

pub(super) fn ensure_drop_type(object_type: ObjectType, meta: &RelationMeta) -> Result<(), String> {
    let ok = match object_type {
        ObjectType::Table => meta.relkind == "r" || meta.relkind == "p",
        ObjectType::View => meta.relkind == "v",
        ObjectType::Index => meta.relkind == "i",
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "DROP {object_type} cannot drop relation with relkind={}",
            meta.relkind
        ))
    }
}

pub(super) fn drop_relation_dependencies(
    conn: &Connection,
    meta: &RelationMeta,
    cascade: bool,
) -> Result<(), String> {
    let dependent_relations = dependent_relation_oids(conn, meta.oid)?;
    let dependent_constraints = dependent_constraint_oids(conn, meta.oid)?;
    if (!dependent_relations.is_empty() || !dependent_constraints.is_empty()) && !cascade {
        return Err("cannot drop relation with dependent objects without CASCADE".into());
    }
    for constraint_oid in dependent_constraints {
        delete_constraint_catalog(conn, constraint_oid)?;
    }
    for dependent_oid in dependent_relations {
        if let Some(dependent) = relation_meta_by_oid(conn, dependent_oid)? {
            delete_relation_catalog(conn, &dependent)?;
        }
    }
    Ok(())
}

pub(super) fn dependent_relation_oids(conn: &Connection, rel_oid: i64) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT objid FROM rsduck_catalog.rs_dependency \
             WHERE refclassid = {OBJECT_RELATION_KIND} AND refobjid = {rel_oid} \
               AND classid = {OBJECT_RELATION_KIND}"
        ))
        .map_err(|e| format!("prepare dependent lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query dependent lookup failed: {e}"))?;
    let mut oids = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read dependent lookup failed: {e}"))?
    {
        oids.push(
            row.get(0)
                .map_err(|e| format!("read dependent oid failed: {e}"))?,
        );
    }
    Ok(oids)
}

pub(super) fn dependent_constraint_oids(
    conn: &Connection,
    rel_oid: i64,
) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT d.objid \
             FROM rsduck_catalog.rs_dependency d \
             JOIN rsduck_catalog.rs_constraint con ON con.oid = d.objid \
             WHERE d.refclassid = {OBJECT_RELATION_KIND} \
               AND d.refobjid = {rel_oid} \
               AND d.classid = {OBJECT_CONSTRAINT_KIND} \
               AND con.conrelid <> {rel_oid}"
        ))
        .map_err(|e| format!("prepare dependent constraint lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query dependent constraint lookup failed: {e}"))?;
    let mut oids = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read dependent constraint lookup failed: {e}"))?
    {
        oids.push(
            row.get(0)
                .map_err(|e| format!("read dependent constraint oid failed: {e}"))?,
        );
    }
    Ok(oids)
}

pub(super) fn relation_meta_by_oid(
    conn: &Connection,
    rel_oid: i64,
) -> Result<Option<RelationMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT oid, reltype, relkind, relispartition \
             FROM rsduck_catalog.rs_relation WHERE oid = {rel_oid}"
        ))
        .map_err(|e| format!("prepare relation-by-oid lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query relation-by-oid lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read relation-by-oid lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(RelationMeta {
        oid: row
            .get(0)
            .map_err(|e| format!("read dependent relation oid failed: {e}"))?,
        reltype: row
            .get(1)
            .map_err(|e| format!("read dependent relation type failed: {e}"))?,
        relkind: row
            .get(2)
            .map_err(|e| format!("read dependent relation kind failed: {e}"))?,
        relispartition: row
            .get(3)
            .map_err(|e| format!("read dependent relation partition flag failed: {e}"))?,
    }))
}

pub(super) fn execute_physical_drop(
    conn: &Connection,
    object_type: ObjectType,
    schema: &str,
    relname: &str,
    cascade: bool,
) -> Result<(), String> {
    let keyword = match object_type {
        ObjectType::Table => "TABLE",
        ObjectType::View => "VIEW",
        ObjectType::Index => "INDEX",
        _ => return Err(format!("DROP {object_type} is not supported")),
    };
    let cascade = if cascade { " CASCADE" } else { "" };
    conn.execute(
        &format!(
            "DROP {keyword} {}{cascade}",
            quote_qualified(schema, relname)
        ),
        [],
    )
    .map_err(|e| format!("execute DuckDB DROP {keyword} failed: {e}"))?;
    Ok(())
}

pub(super) fn delete_constraint_catalog(
    conn: &Connection,
    constraint_oid: i64,
) -> Result<(), String> {
    for sql in [
        format!(
            "DELETE FROM rsduck_catalog.rs_dependency \
             WHERE (classid = {OBJECT_CONSTRAINT_KIND} AND objid = {constraint_oid}) \
                OR (refclassid = {OBJECT_CONSTRAINT_KIND} AND refobjid = {constraint_oid})"
        ),
        format!("DELETE FROM rsduck_catalog.rs_comment WHERE objoid = {constraint_oid}"),
        format!("DELETE FROM rsduck_catalog.rs_constraint WHERE oid = {constraint_oid}"),
    ] {
        conn.execute(&sql, [])
            .map_err(|e| format!("delete constraint catalog rows failed: {e}"))?;
    }
    Ok(())
}

pub(super) fn delete_relation_catalog(
    conn: &Connection,
    meta: &RelationMeta,
) -> Result<(), String> {
    let table_oid: Option<i64> = if meta.relkind == "i" {
        conn.query_row(
            &format!(
                "SELECT indrelid FROM rsduck_catalog.rs_index WHERE indexrelid = {}",
                meta.oid
            ),
            [],
            |row| row.get(0),
        )
        .ok()
    } else {
        None
    };

    for sql in [
        format!(
            "DELETE FROM rsduck_catalog.rs_dependency WHERE objid = {} OR refobjid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_comment WHERE objoid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_column_default WHERE adrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_column WHERE attrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_constraint WHERE conrelid = {} OR conindid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_vector_index WHERE indexrelid = {} OR indexrelid IN (SELECT indexrelid FROM rsduck_catalog.rs_index WHERE indrelid = {})",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_index WHERE indexrelid = {} OR indrelid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_relation_ext WHERE relid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_partition WHERE parent_relid = {} OR child_relid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_type WHERE oid = {} OR typrelid = {}",
            meta.reltype, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_relation WHERE oid = {}",
            meta.oid
        ),
    ] {
        conn.execute(&sql, [])
            .map_err(|e| format!("delete relation catalog rows failed: {e}"))?;
    }

    if let Some(table_oid) = table_oid {
        let index_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM rsduck_catalog.rs_index WHERE indrelid = {table_oid}"
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count remaining indexes failed: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_relation SET relhasindex = {} WHERE oid = {table_oid}",
                sql_bool(index_count > 0)
            ),
            [],
        )
        .map_err(|e| format!("update relhasindex after drop failed: {e}"))?;
    }
    Ok(())
}

pub(super) fn catalog_exists(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = 'rsduck_catalog' AND table_name = 'rs_catalog_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check catalog existence failed: {e}"))?;
    Ok(count > 0)
}

pub(super) fn catalog_version_row_exists(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check catalog version row failed: {e}"))?;
    Ok(count > 0)
}

pub(super) fn has_user_objects(conn: &Connection) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_tables() \
             WHERE internal = FALSE \
               AND schema_name NOT IN ('information_schema', 'pg_catalog', 'rsduck_catalog', 'rsduck_internal')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check existing DuckDB user objects failed: {e}"))?;
    Ok(count > 0)
}

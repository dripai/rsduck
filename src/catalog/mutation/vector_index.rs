use super::*;

pub(crate) fn create_vector_index(
    conn: &Connection,
    request: &VectorIndexCreateRequest,
    owner_user_id: i64,
) -> Result<VectorIndexStatus, String> {
    validate_vector_index_request(request)?;
    let extension_version = loaded_vss_version(conn)?;

    run_catalog_tx(conn, || {
        if vector_space_exists(conn, &request.vector_space)? {
            return Err(format!(
                "vector space already exists: {}",
                request.vector_space
            ));
        }
        if relation_exists(conn, &request.schema, &request.index_name)? {
            return Err(format!(
                "relation already exists: {}.{}",
                request.schema, request.index_name
            ));
        }

        let table_oid = relation_oid(conn, &request.schema, &request.table)?;
        if relation_kind(conn, table_oid)? != "r" {
            return Err("HNSW vector indexes only support ordinary tables".into());
        }
        let columns = catalog_columns(conn, table_oid)?;
        validate_vector_table_contract(conn, &request.schema, &request.table, &columns)?;
        let column = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(&request.column))
            .ok_or_else(|| format!("vector index column does not exist: {}", request.column))?;
        let dimension = fixed_float_array_dimension(&column.duckdb_type)?;
        let vector_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE {} IS NOT NULL",
                    quote_qualified(&request.schema, &request.table),
                    quote_ident(&request.column)
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count vector index rows failed: {e}"))?;

        let index_oid = allocate_oid(conn)?;
        let request_json = serde_json::json!({
            "vector_space": request.vector_space,
            "schema": request.schema,
            "table": request.table,
            "column": request.column,
            "index_name": request.index_name,
            "metric": request.metric,
            "m": request.m,
            "m0": request.m0,
            "ef_construction": request.ef_construction,
            "default_ef_search": request.default_ef_search,
        })
        .to_string();
        let journal_id = insert_journal(conn, "create_vector_index", index_oid, &request_json)?;

        let namespace_oid = namespace_oid(conn, &request.schema)?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_relation(oid, relname, relnamespace, reltype, relowner, \
                 relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message) \
                 VALUES ({index_oid}, '{}', {namespace_oid}, 0, {owner_user_id}, 'i', 'p', 1, 0, FALSE, FALSE, '', 'hnsw', 'active', '')",
                sql_string(&request.index_name)
            ),
            [],
        )
        .map_err(|e| format!("write HNSW rs_relation failed: {e}"))?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_index(indexrelid, indrelid, indnatts, indnkeyatts, indisunique, indisprimary, indisvalid, indkey, indexprs, indpred) \
                 VALUES ({index_oid}, {table_oid}, 1, 1, FALSE, FALSE, TRUE, '{}', '', '')",
                column.attnum
            ),
            [],
        )
        .map_err(|e| format!("write HNSW rs_index failed: {e}"))?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_vector_index(indexrelid, vector_space, embedding_model, model_version, dimension, metric, m, m0, ef_construction, default_ef_search, definition_version, generation, extension_version, build_status, vector_count, built_at, updated_at, error_message) \
                 VALUES ({index_oid}, '{}', '{}', '{}', {dimension}, '{}', {}, {}, {}, {}, 1, 1, '{}', 'pending', {vector_count}, NULL, CURRENT_TIMESTAMP, '')",
                sql_string(&request.vector_space),
                sql_string(&request.embedding_model),
                sql_string(&request.model_version),
                sql_string(&request.metric),
                request.m,
                request.m0,
                request.ef_construction,
                request.default_ef_search,
                sql_string(&extension_version)
            ),
            [],
        )
        .map_err(|e| format!("write rs_vector_index failed: {e}"))?;
        conn.execute(
            &format!(
                "INSERT INTO rsduck_catalog.rs_dependency(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
                 VALUES ({OBJECT_RELATION_KIND}, {index_oid}, 0, {OBJECT_RELATION_KIND}, {table_oid}, {}, 'n')",
                column.attnum
            ),
            [],
        )
        .map_err(|e| format!("write HNSW dependency failed: {e}"))?;
        transition_vector_index_status(conn, index_oid, "building", "")?;
        conn.execute_batch(&vector_index_create_sql(request))
            .map_err(|e| format!("execute DuckDB CREATE HNSW INDEX failed: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_vector_index SET built_at = CURRENT_TIMESTAMP
                 WHERE indexrelid = {index_oid}"
            ),
            [],
        )
        .map_err(|e| format!("record HNSW build time failed: {e}"))?;
        transition_vector_index_status(conn, index_oid, "active", "")?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_relation SET relhasindex = TRUE WHERE oid = {table_oid}"
            ),
            [],
        )
        .map_err(|e| format!("update vector table relhasindex failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        vector_index_status(conn, &request.vector_space)
    })
}

fn validate_vector_table_contract(
    conn: &Connection,
    schema: &str,
    table: &str,
    columns: &[CatalogColumn],
) -> Result<(), String> {
    for (name, expected_type) in [
        ("tenant_id", "BIGINT"),
        ("agent_id", "BIGINT"),
        ("memory_id", "BIGINT"),
        ("source_version", "BIGINT"),
        ("content_hash", "VARCHAR"),
        ("updated_at", "TIMESTAMP"),
    ] {
        let column = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("vector table requires column: {name}"))?;
        if !column.duckdb_type.eq_ignore_ascii_case(expected_type) {
            return Err(format!(
                "vector table column {name} must use {expected_type}, got {}",
                column.duckdb_type
            ));
        }
    }
    let unique_key_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM duckdb_constraints()
                 WHERE schema_name = '{}' AND table_name = '{}'
                   AND constraint_type IN ('PRIMARY KEY', 'UNIQUE')
                   AND constraint_column_names = ['tenant_id', 'agent_id', 'memory_id']",
                sql_string(schema),
                sql_string(table)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("inspect vector table unique key failed: {e}"))?;
    if unique_key_count != 1 {
        return Err(
            "INVALID_VECTOR_TABLE: vector table requires UNIQUE(tenant_id, agent_id, memory_id)"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn vector_index_status(
    conn: &Connection,
    vector_space: &str,
) -> Result<VectorIndexStatus, String> {
    let vector_space = vector_space.trim();
    if vector_space.is_empty() {
        return Err("vector_space cannot be empty".into());
    }
    conn.query_row(
        &format!(
            "SELECT v.indexrelid, v.vector_space, n.nspname, t.relname, a.attname, i.relname,
                    v.embedding_model, v.model_version, v.dimension, v.metric, v.m, v.m0,
                    v.ef_construction, v.default_ef_search, v.definition_version, v.generation,
                    v.extension_version, v.build_status, v.vector_count,
                    COALESCE(CAST(v.built_at AS VARCHAR), ''),
                    CAST(v.updated_at AS VARCHAR), v.error_message
             FROM rsduck_catalog.rs_vector_index v
             JOIN rsduck_catalog.rs_index x ON x.indexrelid = v.indexrelid
             JOIN rsduck_catalog.rs_relation i ON i.oid = v.indexrelid
             JOIN rsduck_catalog.rs_relation t ON t.oid = x.indrelid
             JOIN rsduck_catalog.rs_schema n ON n.oid = t.relnamespace
             JOIN rsduck_catalog.rs_column a ON a.attrelid = t.oid AND CAST(a.attnum AS VARCHAR) = x.indkey
             WHERE v.vector_space = '{}'",
            sql_string(vector_space)
        ),
        [],
        |row| {
            Ok(VectorIndexStatus {
                index_oid: row.get(0)?,
                vector_space: row.get(1)?,
                schema: row.get(2)?,
                table: row.get(3)?,
                column: row.get(4)?,
                index_name: row.get(5)?,
                embedding_model: row.get(6)?,
                model_version: row.get(7)?,
                dimension: row.get::<_, i32>(8)? as usize,
                metric: row.get(9)?,
                m: row.get(10)?,
                m0: row.get(11)?,
                ef_construction: row.get(12)?,
                default_ef_search: row.get(13)?,
                definition_version: row.get(14)?,
                generation: row.get(15)?,
                extension_version: row.get(16)?,
                build_status: row.get(17)?,
                vector_count: row.get(18)?,
                built_at: row.get(19)?,
                updated_at: row.get(20)?,
                error_message: row.get(21)?,
            })
        },
    )
    .map_err(|e| format!("vector space not found: {vector_space}: {e}"))
}

pub(crate) fn rebuild_vector_index(
    conn: &Connection,
    vector_space: &str,
) -> Result<VectorIndexStatus, String> {
    let status = vector_index_status(conn, vector_space)?;
    transition_vector_index_status(conn, status.index_oid, "rebuilding", "")?;
    let request = vector_index_request_from_status(&status);
    let result = run_catalog_tx(conn, || {
        let journal_id =
            insert_journal(conn, "rebuild_vector_index", status.index_oid, vector_space)?;
        conn.execute_batch(&format!(
            "DROP INDEX IF EXISTS {}",
            quote_ident(&status.index_name)
        ))
        .map_err(|e| format!("drop HNSW index for rebuild failed: {e}"))?;
        conn.execute_batch(&vector_index_create_sql(&request))
            .map_err(|e| format!("recreate HNSW index failed: {e}"))?;
        let vector_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE {} IS NOT NULL",
                    quote_qualified(&status.schema, &status.table),
                    quote_ident(&status.column)
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count rebuilt vector rows failed: {e}"))?;
        let extension_version = loaded_vss_version(conn)?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_vector_index
                 SET generation = generation + 1, extension_version = '{}', vector_count = {vector_count}, built_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
                 WHERE indexrelid = {}",
                sql_string(&extension_version),
                status.index_oid
            ),
            [],
        )
        .map_err(|e| format!("mark rebuilt vector index active failed: {e}"))?;
        transition_vector_index_status(conn, status.index_oid, "active", "")?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_relation
                 SET status = 'active', error_message = '' WHERE oid = {}",
                status.index_oid
            ),
            [],
        )
        .map_err(|e| format!("mark rebuilt index relation active failed: {e}"))?;
        finish_journal(conn, journal_id)?;
        Ok(())
    });
    if let Err(error) = result {
        transition_vector_index_status(conn, status.index_oid, "failed", &error)?;
        return Err(error);
    }
    vector_index_status(conn, vector_space)
}

pub(crate) fn compact_vector_index(
    conn: &Connection,
    vector_space: &str,
) -> Result<VectorIndexStatus, String> {
    let status = vector_index_status(conn, vector_space)?;
    if status.build_status != "active" {
        return Err(format!(
            "INDEX_UNAVAILABLE: vector space {vector_space} status={}",
            status.build_status
        ));
    }
    transition_vector_index_status(conn, status.index_oid, "compacting", "")?;
    let result = conn
        .execute_batch(&format!(
            "PRAGMA hnsw_compact_index('{}')",
            sql_string(&status.index_name)
        ))
        .map_err(|e| format!("compact HNSW index failed: {e}"));
    if let Err(error) = result {
        transition_vector_index_status(conn, status.index_oid, "failed", &error)?;
        return Err(error);
    }
    transition_vector_index_status(conn, status.index_oid, "active", "")?;
    vector_index_status(conn, vector_space)
}

fn vector_index_request_from_status(status: &VectorIndexStatus) -> VectorIndexCreateRequest {
    VectorIndexCreateRequest {
        vector_space: status.vector_space.clone(),
        schema: status.schema.clone(),
        table: status.table.clone(),
        column: status.column.clone(),
        index_name: status.index_name.clone(),
        embedding_model: status.embedding_model.clone(),
        model_version: status.model_version.clone(),
        metric: status.metric.clone(),
        m: status.m,
        m0: status.m0,
        ef_construction: status.ef_construction,
        default_ef_search: status.default_ef_search,
    }
}

pub(crate) fn transition_vector_index_status(
    conn: &Connection,
    index_oid: i64,
    next_status: &str,
    error_message: &str,
) -> Result<(), String> {
    let current_status: String = conn
        .query_row(
            &format!(
                "SELECT build_status FROM rsduck_catalog.rs_vector_index WHERE indexrelid = {index_oid}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read vector index runtime status failed: {e}"))?;
    if !vector_index_transition_allowed(&current_status, next_status) {
        return Err(format!(
            "INVALID_INDEX_STATE_TRANSITION: {current_status} -> {next_status}"
        ));
    }
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_vector_index
             SET build_status = '{}', updated_at = CURRENT_TIMESTAMP, error_message = '{}'
             WHERE indexrelid = {index_oid}",
            sql_string(next_status),
            sql_string(error_message)
        ),
        [],
    )
    .map_err(|e| format!("update vector index runtime status failed: {e}"))?;
    refresh_catalog_checksum(conn)
}

fn vector_index_transition_allowed(current: &str, next: &str) -> bool {
    matches!(
        (current, next),
        ("pending", "building")
            | ("pending", "failed")
            | ("pending", "unavailable")
            | ("building", "active")
            | ("building", "failed")
            | ("building", "unavailable")
            | ("active", "rebuilding")
            | ("active", "compacting")
            | ("active", "stale")
            | ("active", "unavailable")
            | ("rebuilding", "active")
            | ("rebuilding", "failed")
            | ("rebuilding", "unavailable")
            | ("compacting", "active")
            | ("compacting", "failed")
            | ("compacting", "unavailable")
            | ("stale", "rebuilding")
            | ("stale", "unavailable")
            | ("failed", "rebuilding")
            | ("failed", "unavailable")
            | ("unavailable", "rebuilding")
    )
}

pub(crate) fn vector_index_create_sql(request: &VectorIndexCreateRequest) -> String {
    format!(
        "CREATE INDEX {} ON {} USING HNSW ({}) WITH (metric = '{}', ef_construction = {}, M = {}, M0 = {})",
        quote_ident(&request.index_name),
        quote_qualified(&request.schema, &request.table),
        quote_ident(&request.column),
        request.metric,
        request.ef_construction,
        request.m,
        request.m0
    )
}

fn validate_vector_index_request(request: &VectorIndexCreateRequest) -> Result<(), String> {
    for (name, value) in [
        ("vector_space", request.vector_space.as_str()),
        ("schema", request.schema.as_str()),
        ("table", request.table.as_str()),
        ("column", request.column.as_str()),
        ("index_name", request.index_name.as_str()),
        ("embedding_model", request.embedding_model.as_str()),
        ("model_version", request.model_version.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{name} cannot be empty"));
        }
    }
    if !matches!(request.metric.as_str(), "cosine" | "l2sq" | "ip") {
        return Err(format!(
            "unsupported vector distance metric: {}",
            request.metric
        ));
    }
    if request.m <= 0 || request.m0 < request.m {
        return Err("HNSW parameters require M > 0 and M0 >= M".into());
    }
    if request.ef_construction <= 0 || request.default_ef_search <= 0 {
        return Err("HNSW ef_construction and default_ef_search must be greater than zero".into());
    }
    Ok(())
}

fn vector_space_exists(conn: &Connection, vector_space: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM rsduck_catalog.rs_vector_index WHERE vector_space = '{}'",
                sql_string(vector_space)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check vector space existence failed: {e}"))?;
    Ok(count > 0)
}

fn loaded_vss_version(conn: &Connection) -> Result<String, String> {
    let (loaded, version): (bool, String) = conn
        .query_row(
            "SELECT loaded, COALESCE(extension_version, '') FROM duckdb_extensions() WHERE extension_name = 'vss'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("read DuckDB VSS extension status failed: {e}"))?;
    if !loaded {
        return Err("DuckDB VSS extension is not loaded".into());
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::vector_index_transition_allowed;

    #[test]
    fn vector_index_state_machine_covers_declared_lifecycle() {
        assert!(vector_index_transition_allowed("pending", "building"));
        assert!(vector_index_transition_allowed("building", "active"));
        assert!(vector_index_transition_allowed("active", "rebuilding"));
        assert!(vector_index_transition_allowed("rebuilding", "active"));
        assert!(vector_index_transition_allowed("active", "compacting"));
        assert!(vector_index_transition_allowed("compacting", "active"));
        assert!(vector_index_transition_allowed("active", "stale"));
        assert!(vector_index_transition_allowed("stale", "rebuilding"));
        assert!(vector_index_transition_allowed("active", "unavailable"));
        assert!(vector_index_transition_allowed("unavailable", "rebuilding"));
        assert!(vector_index_transition_allowed("rebuilding", "failed"));
        assert!(vector_index_transition_allowed("failed", "rebuilding"));
        assert!(!vector_index_transition_allowed("active", "building"));
        assert!(!vector_index_transition_allowed("failed", "active"));
    }
}

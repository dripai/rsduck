use super::*;
use std::collections::HashSet;

const MAX_VECTOR_TOP_K: usize = 1_000;
const MAX_VECTOR_BATCH_SIZE: usize = 1_000;

pub(crate) fn search_vectors_blocking(
    conn: &Connection,
    request: &VectorSearchRequest,
) -> Result<VectorSearchResult, String> {
    validate_search_request(request)?;
    let index = crate::catalog::vector_index_status(conn, &request.vector_space)?;
    if request.mode == "ann" && index.build_status != "active" {
        return Err(index_status_error(&index));
    }
    if request.embedding.len() != index.dimension {
        return Err(format!(
            "VECTOR_DIMENSION_MISMATCH: expected={}, actual={}",
            index.dimension,
            request.embedding.len()
        ));
    }
    if index.metric == "cosine" && request.embedding.iter().all(|value| *value == 0.0) {
        return Err("INVALID_VECTOR_VALUE: cosine query vector cannot be all zero".into());
    }

    let vector = format!(
        "{}::FLOAT[{}]",
        sql_param_literal(&SqlParam::FloatArray(request.embedding.clone()))?,
        index.dimension
    );
    let distance = distance_expression(&index.metric, &request.mode, &index.column, &vector)?;
    let sql = format!(
        "SELECT memory_id, {distance} AS distance
         FROM {}
         WHERE tenant_id = {} AND agent_id = {}
         ORDER BY distance ASC, memory_id ASC
         LIMIT {}",
        vector_quote_qualified(&index.schema, &index.table),
        request.tenant_id,
        request.agent_id,
        request.top_k
    );

    let ann = request.mode == "ann";
    if ann {
        let ef_search = request.ef_search.unwrap_or(index.default_ef_search);
        conn.execute_batch(&format!("SET hnsw_ef_search = {ef_search};"))
            .map_err(|e| format!("set HNSW ef_search failed: {e}"))?;
    }
    let result = query_vector_matches(conn, &sql);
    let reset_result = if ann {
        conn.execute_batch("RESET hnsw_ef_search;")
            .map_err(|e| format!("reset HNSW ef_search failed: {e}"))
    } else {
        Ok(())
    };
    let matches = result?;
    reset_result?;
    Ok(VectorSearchResult {
        vector_space: index.vector_space,
        mode: request.mode.clone(),
        index_status: index.build_status,
        matches,
    })
}

pub(crate) fn upsert_vectors_blocking(
    conn: &Connection,
    request: &VectorUpsertRequest,
) -> Result<VectorMutationResult, String> {
    let index = crate::catalog::vector_index_status(conn, &request.vector_space)?;
    if index.build_status != "active" {
        return Err(index_status_error(&index));
    }
    validate_upsert_request(request, &index)?;
    run_vector_transaction(conn, || {
        let mut applied = 0usize;
        let mut idempotent = 0usize;
        for item in &request.items {
            match existing_vector_version(
                conn,
                &index,
                item.tenant_id,
                item.agent_id,
                item.memory_id,
            )? {
                Some((stored_version, _)) if stored_version > item.source_version => {
                    return Err(format!(
                        "STALE_SOURCE_VERSION: memory_id={}, stored={}, incoming={}",
                        item.memory_id, stored_version, item.source_version
                    ));
                }
                Some((stored_version, stored_hash)) if stored_version == item.source_version => {
                    if stored_hash != item.content_hash {
                        return Err(format!(
                            "SOURCE_VERSION_CONFLICT: memory_id={} has different content_hash for version {}",
                            item.memory_id, item.source_version
                        ));
                    }
                    idempotent += 1;
                    continue;
                }
                Some(_) => {
                    delete_vector_row(conn, &index, item.tenant_id, item.agent_id, item.memory_id)?
                }
                None => {}
            }
            let vector = format!(
                "{}::FLOAT[{}]",
                sql_param_literal(&SqlParam::FloatArray(item.embedding.clone()))?,
                index.dimension
            );
            conn.execute(
                &format!(
                    "INSERT INTO {} (tenant_id, agent_id, memory_id, source_version, content_hash, {}, updated_at)
                     VALUES ({}, {}, {}, {}, {}, {}, CURRENT_TIMESTAMP)",
                    vector_quote_qualified(&index.schema, &index.table),
                    vector_quote_ident(&index.column),
                    item.tenant_id,
                    item.agent_id,
                    item.memory_id,
                    item.source_version,
                    sql_string_literal(&item.content_hash),
                    vector
                ),
                [],
            )
            .map_err(|e| format!("insert vector row failed: {e}"))?;
            applied += 1;
        }
        let vector_count = update_vector_count(conn, &index)?;
        Ok(VectorMutationResult {
            vector_space: index.vector_space.clone(),
            applied,
            idempotent,
            vector_count,
        })
    })
}

pub(crate) fn delete_vectors_blocking(
    conn: &Connection,
    request: &VectorDeleteRequest,
) -> Result<VectorMutationResult, String> {
    let index = crate::catalog::vector_index_status(conn, &request.vector_space)?;
    if index.build_status != "active" {
        return Err(index_status_error(&index));
    }
    validate_delete_request(request)?;
    run_vector_transaction(conn, || {
        let mut applied = 0usize;
        let mut idempotent = 0usize;
        for item in &request.items {
            match existing_vector_version(
                conn,
                &index,
                item.tenant_id,
                item.agent_id,
                item.memory_id,
            )? {
                Some((stored_version, _)) if stored_version > item.source_version => {
                    return Err(format!(
                        "STALE_SOURCE_VERSION: memory_id={}, stored={}, incoming={}",
                        item.memory_id, stored_version, item.source_version
                    ));
                }
                Some(_) => {
                    delete_vector_row(conn, &index, item.tenant_id, item.agent_id, item.memory_id)?;
                    applied += 1;
                }
                None => idempotent += 1,
            }
        }
        let vector_count = update_vector_count(conn, &index)?;
        Ok(VectorMutationResult {
            vector_space: index.vector_space.clone(),
            applied,
            idempotent,
            vector_count,
        })
    })
}

fn run_vector_transaction<T>(
    conn: &Connection,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("begin vector mutation failed: {e}"))?;
    match operation() {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("commit vector mutation failed: {e}"))?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn index_status_error(index: &crate::catalog::VectorIndexStatus) -> String {
    let code = match index.build_status.as_str() {
        "pending" | "building" | "rebuilding" => "INDEX_BUILDING",
        "stale" => "INDEX_STALE",
        _ => "INDEX_UNAVAILABLE",
    };
    format!(
        "{code}: vector space {} status={}",
        index.vector_space, index.build_status
    )
}

fn existing_vector_version(
    conn: &Connection,
    index: &crate::catalog::VectorIndexStatus,
    tenant_id: i64,
    agent_id: i64,
    memory_id: i64,
) -> Result<Option<(i64, String)>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT source_version, content_hash FROM {}
             WHERE tenant_id = {tenant_id} AND agent_id = {agent_id} AND memory_id = {memory_id}",
            vector_quote_qualified(&index.schema, &index.table)
        ))
        .map_err(|e| format!("prepare vector version lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query vector version failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read vector version failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some((
        row.get(0)
            .map_err(|e| format!("read source_version failed: {e}"))?,
        row.get(1)
            .map_err(|e| format!("read content_hash failed: {e}"))?,
    )))
}

fn delete_vector_row(
    conn: &Connection,
    index: &crate::catalog::VectorIndexStatus,
    tenant_id: i64,
    agent_id: i64,
    memory_id: i64,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "DELETE FROM {} WHERE tenant_id = {tenant_id} AND agent_id = {agent_id} AND memory_id = {memory_id}",
            vector_quote_qualified(&index.schema, &index.table)
        ),
        [],
    )
    .map_err(|e| format!("delete vector row failed: {e}"))?;
    Ok(())
}

fn update_vector_count(
    conn: &Connection,
    index: &crate::catalog::VectorIndexStatus,
) -> Result<i64, String> {
    let vector_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM {} WHERE {} IS NOT NULL",
                vector_quote_qualified(&index.schema, &index.table),
                vector_quote_ident(&index.column)
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("count vector rows failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_vector_index SET vector_count = {vector_count}, updated_at = CURRENT_TIMESTAMP WHERE indexrelid = {}",
            index.index_oid
        ),
        [],
    )
    .map_err(|e| format!("update vector count failed: {e}"))?;
    crate::catalog::refresh_catalog_checksum(conn)?;
    Ok(vector_count)
}

fn validate_upsert_request(
    request: &VectorUpsertRequest,
    index: &crate::catalog::VectorIndexStatus,
) -> Result<(), String> {
    validate_batch(&request.vector_space, request.items.len())?;
    let mut keys = HashSet::new();
    for item in &request.items {
        if !keys.insert((item.tenant_id, item.agent_id, item.memory_id)) {
            return Err(format!(
                "DUPLICATE_VECTOR_KEY: memory_id={} appears more than once in the batch",
                item.memory_id
            ));
        }
        if item.source_version < 0 {
            return Err("INVALID_SOURCE_VERSION: source_version cannot be negative".into());
        }
        if item.content_hash.trim().is_empty() {
            return Err("INVALID_CONTENT_HASH: content_hash cannot be empty".into());
        }
        if item.embedding.len() != index.dimension {
            return Err(format!(
                "VECTOR_DIMENSION_MISMATCH: memory_id={}, expected={}, actual={}",
                item.memory_id,
                index.dimension,
                item.embedding.len()
            ));
        }
        if let Some(value) = item.embedding.iter().find(|value| !value.is_finite()) {
            return Err(format!(
                "INVALID_VECTOR_VALUE: memory_id={} contains non-finite value: {value}",
                item.memory_id
            ));
        }
        if index.metric == "cosine" && item.embedding.iter().all(|value| *value == 0.0) {
            return Err(format!(
                "INVALID_VECTOR_VALUE: memory_id={} cosine vector cannot be all zero",
                item.memory_id
            ));
        }
    }
    Ok(())
}

fn validate_delete_request(request: &VectorDeleteRequest) -> Result<(), String> {
    validate_batch(&request.vector_space, request.items.len())?;
    let mut keys = HashSet::new();
    for item in &request.items {
        if !keys.insert((item.tenant_id, item.agent_id, item.memory_id)) {
            return Err(format!(
                "DUPLICATE_VECTOR_KEY: memory_id={} appears more than once in the batch",
                item.memory_id
            ));
        }
        if item.source_version < 0 {
            return Err("INVALID_SOURCE_VERSION: source_version cannot be negative".into());
        }
    }
    Ok(())
}

fn validate_batch(vector_space: &str, len: usize) -> Result<(), String> {
    if vector_space.trim().is_empty() {
        return Err("VECTOR_SPACE_NOT_FOUND: vector_space cannot be empty".into());
    }
    if len == 0 || len > MAX_VECTOR_BATCH_SIZE {
        return Err(format!(
            "INVALID_BATCH_SIZE: batch size must be between 1 and {MAX_VECTOR_BATCH_SIZE}"
        ));
    }
    Ok(())
}

fn query_vector_matches(conn: &Connection, sql: &str) -> Result<Vec<VectorMatch>, String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare vector search failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(VectorMatch {
                memory_id: row.get(0)?,
                distance: row.get(1)?,
            })
        })
        .map_err(|e| format!("execute vector search failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read vector search result failed: {e}"))
}

fn validate_search_request(request: &VectorSearchRequest) -> Result<(), String> {
    if request.vector_space.trim().is_empty() {
        return Err("VECTOR_SPACE_NOT_FOUND: vector_space cannot be empty".into());
    }
    if request.embedding.is_empty() {
        return Err("INVALID_VECTOR_VALUE: embedding cannot be empty".into());
    }
    if let Some(value) = request.embedding.iter().find(|value| !value.is_finite()) {
        return Err(format!(
            "INVALID_VECTOR_VALUE: embedding contains non-finite value: {value}"
        ));
    }
    if request.top_k == 0 || request.top_k > MAX_VECTOR_TOP_K {
        return Err(format!(
            "INVALID_TOP_K: top_k must be between 1 and {MAX_VECTOR_TOP_K}"
        ));
    }
    if !matches!(request.mode.as_str(), "ann" | "exact") {
        return Err("INVALID_SEARCH_MODE: mode must be ann or exact".into());
    }
    if request.ef_search.is_some_and(|value| value <= 0) {
        return Err("INVALID_EF_SEARCH: ef_search must be greater than zero".into());
    }
    Ok(())
}

fn distance_expression(
    metric: &str,
    mode: &str,
    column: &str,
    vector: &str,
) -> Result<String, String> {
    let column = vector_quote_ident(column);
    let function = match (metric, mode) {
        ("cosine", "ann") => "array_cosine_distance",
        ("l2sq", "ann") => "array_distance",
        ("ip", "ann") => "array_negative_inner_product",
        ("cosine", "exact") => "list_cosine_distance",
        ("l2sq", "exact") => "list_distance",
        ("ip", "exact") => "list_negative_inner_product",
        _ => {
            return Err(format!(
                "unsupported vector metric or mode: {metric}/{mode}"
            ))
        }
    };
    Ok(format!("{function}({column}, {vector})"))
}

fn vector_quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn vector_quote_qualified(schema: &str, relation: &str) -> String {
    format!(
        "{}.{}",
        vector_quote_ident(schema),
        vector_quote_ident(relation)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_search_request_validation_is_strict() {
        let request = VectorSearchRequest {
            vector_space: "space".into(),
            tenant_id: 1,
            agent_id: 2,
            embedding: vec![0.1, 0.2],
            top_k: 10,
            mode: "ann".into(),
            ef_search: Some(64),
        };
        validate_search_request(&request).unwrap();

        let mut invalid = request.clone();
        invalid.mode = "fallback".into();
        assert!(validate_search_request(&invalid)
            .unwrap_err()
            .contains("INVALID_SEARCH_MODE"));
        invalid = request.clone();
        invalid.embedding[0] = f32::NAN;
        assert!(validate_search_request(&invalid)
            .unwrap_err()
            .contains("INVALID_VECTOR_VALUE"));
    }
}

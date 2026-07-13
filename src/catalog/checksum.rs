use super::*;

pub(super) fn validate_catalog_checksum(conn: &Connection) -> Result<(), String> {
    let expected: String = conn
        .query_row(
            "SELECT catalog_checksum FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read catalog checksum failed: {e}"))?;
    let actual = calculate_catalog_checksum(conn)?;
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "catalog checksum mismatch: expected={expected}, actual={actual}"
        ))
    }
}

pub(crate) fn refresh_catalog_checksum(conn: &Connection) -> Result<(), String> {
    let checksum = calculate_catalog_checksum(conn)?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_catalog_version \
             SET catalog_checksum = '{}', updated_at = CURRENT_TIMESTAMP \
             WHERE id = 1",
            sql_string(&checksum)
        ),
        [],
    )
    .map_err(|e| format!("update catalog checksum failed: {e}"))?;
    Ok(())
}

pub(super) fn calculate_catalog_checksum(conn: &Connection) -> Result<String, String> {
    let mut state = FNV64_OFFSET;
    for (label, sql) in catalog_checksum_queries() {
        hash_checksum_part(&mut state, label);
        hash_query_rows(conn, &mut state, sql)?;
    }
    Ok(format!("fnv1a64:{state:016x}"))
}

pub(super) fn catalog_checksum_queries() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "rs_catalog_version",
            "SELECT id, schema_version, snapshot_format_version, catalog_epoch, status \
             FROM rsduck_catalog.rs_catalog_version ORDER BY id",
        ),
        (
            "rs_oid_alloc",
            "SELECT id, next_oid FROM rsduck_catalog.rs_oid_alloc ORDER BY id",
        ),
        (
            "rs_catalog_journal",
            "SELECT journal_id, catalog_epoch, mutation_type, target_oid, request_json, status, error_message \
             FROM rsduck_catalog.rs_catalog_journal ORDER BY journal_id",
        ),
        (
            "rs_schema",
            "SELECT oid, nspname, nspowner, nspacl \
             FROM rsduck_catalog.rs_schema ORDER BY oid",
        ),
        (
            "rs_type",
            "SELECT oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typisdefined, typrelid, typelem, typarray, rsduck_physical_type \
             FROM rsduck_catalog.rs_type ORDER BY oid",
        ),
        (
            "rs_relation",
            "SELECT oid, relname, relnamespace, reltype, relowner, relkind, relpersistence, relnatts, reltuples, relhasindex, relispartition, relpartbound, reloptions, status, error_message \
             FROM rsduck_catalog.rs_relation ORDER BY oid",
        ),
        (
            "rs_column",
            "SELECT attrelid, attname, atttypid, attnum, atttypmod, attnotnull, atthasdef, attisdropped, attidentity, attgenerated, attoptions \
             FROM rsduck_catalog.rs_column ORDER BY attrelid, attnum",
        ),
        (
            "rs_column_default",
            "SELECT oid, adrelid, adnum, adbin \
             FROM rsduck_catalog.rs_column_default ORDER BY oid",
        ),
        (
            "rs_constraint",
            "SELECT oid, conname, connamespace, contype, conrelid, conindid, conkey, confrelid, confkey, convalidated, conbin \
             FROM rsduck_catalog.rs_constraint ORDER BY oid",
        ),
        (
            "rs_index",
            "SELECT indexrelid, indrelid, indnatts, indnkeyatts, indisunique, indisprimary, indisvalid, indkey, indexprs, indpred \
             FROM rsduck_catalog.rs_index ORDER BY indexrelid",
        ),
        (
            "rs_vector_index",
            "SELECT indexrelid, vector_space, embedding_model, model_version, dimension, metric, m, m0, ef_construction, default_ef_search, definition_version, generation, extension_version, build_status, vector_count, error_message \
             FROM rsduck_catalog.rs_vector_index ORDER BY indexrelid",
        ),
        (
            "rs_dependency",
            "SELECT classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype \
             FROM rsduck_catalog.rs_dependency ORDER BY classid, objid, objsubid, refclassid, refobjid, refobjsubid",
        ),
        (
            "rs_comment",
            "SELECT objoid, classoid, objsubid, description \
             FROM rsduck_catalog.rs_comment ORDER BY objoid, classoid, objsubid",
        ),
        (
            "rs_relation_ext",
            "SELECT relid, managed_kind, storage_mode, visibility, partition_key, partition_key_type, partition_unit, retention_count, generated_sql, properties_json \
             FROM rsduck_catalog.rs_relation_ext ORDER BY relid",
        ),
        (
            "rs_partition",
            "SELECT parent_relid, child_relid, partition_value, partition_unit, lower_bound, upper_bound, is_null_partition, status, row_count, min_ts, max_ts, checksum, error_message \
             FROM rsduck_catalog.rs_partition ORDER BY parent_relid, child_relid",
        ),
        (
            "rs_user",
            "SELECT user_id, username, password_hash, password_algo, mysql_auth_plugin, mysql_auth_string, status, is_builtin \
             FROM rsduck_catalog.rs_user ORDER BY user_id",
        ),
        (
            "rs_role",
            "SELECT role_id, role_name, description, is_builtin \
             FROM rsduck_catalog.rs_role ORDER BY role_id",
        ),
        (
            "rs_user_role",
            "SELECT user_id, role_id, granted_by \
             FROM rsduck_catalog.rs_user_role ORDER BY user_id, role_id",
        ),
        (
            "rs_privilege",
            "SELECT privilege_id, principal_type, principal_id, object_type, object_id, action, granted_by \
             FROM rsduck_catalog.rs_privilege ORDER BY privilege_id",
        ),
    ]
}

pub(super) fn hash_query_rows(conn: &Connection, state: &mut u64, sql: &str) -> Result<(), String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare catalog checksum query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query catalog checksum rows failed: {e}"))?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "catalog checksum query did not expose statement metadata".to_string())?;
    let column_count = stmt_ref.column_count();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read catalog checksum row failed: {e}"))?
    {
        hash_checksum_part(state, "row");
        for idx in 0..column_count {
            let value = row
                .get_ref(idx)
                .map_err(|e| format!("read catalog checksum cell failed: {e}"))?;
            hash_checksum_part(state, &checksum_value_to_string(value));
        }
    }
    Ok(())
}

pub(super) fn checksum_value_to_string(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "<null>".to_string(),
        ValueRef::Boolean(v) => v.to_string(),
        ValueRef::TinyInt(v) => v.to_string(),
        ValueRef::SmallInt(v) => v.to_string(),
        ValueRef::Int(v) => v.to_string(),
        ValueRef::BigInt(v) => v.to_string(),
        ValueRef::HugeInt(v) => v.to_string(),
        ValueRef::UTinyInt(v) => v.to_string(),
        ValueRef::USmallInt(v) => v.to_string(),
        ValueRef::UInt(v) => v.to_string(),
        ValueRef::UBigInt(v) => v.to_string(),
        ValueRef::Float(v) => v.to_string(),
        ValueRef::Double(v) => v.to_string(),
        ValueRef::Decimal(v) => v.to_string(),
        ValueRef::Timestamp(unit, value) => format!("{value} {unit:?}"),
        ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
        ValueRef::Blob(v) => format!("<{} bytes>", v.len()),
        ValueRef::Date32(v) => v.to_string(),
        ValueRef::Time64(unit, value) => format!("{value} {unit:?}"),
        ValueRef::Interval {
            months,
            days,
            nanos,
        } => format!("{months} months {days} days {nanos} ns"),
        other => format!("{other:?}"),
    }
}

pub(super) fn hash_checksum_part(state: &mut u64, value: &str) {
    hash_checksum_bytes(state, value.len().to_string().as_bytes());
    hash_checksum_bytes(state, b":");
    hash_checksum_bytes(state, value.as_bytes());
    hash_checksum_bytes(state, b";");
}

pub(super) fn hash_checksum_bytes(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(FNV64_PRIME);
    }
}

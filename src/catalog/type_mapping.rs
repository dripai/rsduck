use super::*;

pub(super) fn type_id_for_duckdb_type(duckdb_type: &str) -> Result<i64, String> {
    let lower = duckdb_type.to_ascii_lowercase();
    if lower == "boolean" || lower == "bool" {
        Ok(TYPE_BOOL)
    } else if lower == "smallint" || lower == "int2" {
        Ok(TYPE_INT2)
    } else if lower == "integer" || lower == "int" || lower == "int4" {
        Ok(TYPE_INT4)
    } else if lower == "bigint" || lower == "int8" {
        Ok(TYPE_INT8)
    } else if lower == "real" || lower == "float" || lower == "float4" {
        Ok(TYPE_FLOAT4)
    } else if lower == "double" || lower == "double precision" || lower == "float8" {
        Ok(TYPE_FLOAT8)
    } else if lower.starts_with("decimal") || lower.starts_with("numeric") {
        Ok(TYPE_NUMERIC)
    } else if lower == "varchar" || lower.starts_with("varchar(") {
        Ok(TYPE_VARCHAR)
    } else if lower == "text" || lower == "string" {
        Ok(TYPE_TEXT)
    } else if lower == "date" {
        Ok(TYPE_DATE)
    } else if lower == "time" || lower.starts_with("time(") {
        Ok(TYPE_TIME)
    } else if lower.starts_with("timestamp") || lower == "datetime" {
        Ok(TYPE_TIMESTAMP)
    } else {
        Err(format!(
            "unsupported DuckDB type for rsduck catalog: {duckdb_type}"
        ))
    }
}

pub(super) fn duckdb_type_for_type_id(conn: &Connection, type_id: i64) -> Result<String, String> {
    conn.query_row(
        &format!("SELECT rsduck_physical_type FROM rsduck_catalog.rs_type WHERE oid = {type_id}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("lookup DuckDB type for catalog type id {type_id} failed: {e}"))
}

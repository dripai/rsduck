fn pg_type_oid_for_duckdb_type(duckdb_type: &str) -> Result<i64, String> {
    let lower = duckdb_type.to_ascii_lowercase();
    if lower == "boolean" || lower == "bool" {
        Ok(16)
    } else if lower == "smallint" || lower == "int2" {
        Ok(21)
    } else if lower == "integer" || lower == "int" || lower == "int4" {
        Ok(23)
    } else if lower == "bigint" || lower == "int8" {
        Ok(20)
    } else if lower == "real" || lower == "float" || lower == "float4" {
        Ok(700)
    } else if lower == "double" || lower == "double precision" || lower == "float8" {
        Ok(701)
    } else if lower.starts_with("decimal") || lower.starts_with("numeric") {
        Ok(1700)
    } else if lower == "varchar" || lower.starts_with("varchar(") {
        Ok(1043)
    } else if lower == "text" || lower == "string" {
        Ok(25)
    } else if lower == "date" {
        Ok(1082)
    } else if lower == "time" || lower.starts_with("time(") {
        Ok(1083)
    } else if lower.starts_with("timestamp") || lower == "datetime" {
        Ok(1114)
    } else {
        Err(format!(
            "unsupported DuckDB type for rsduck catalog: {duckdb_type}"
        ))
    }
}

fn duckdb_type_for_pg_type_oid(conn: &Connection, pg_type_oid: i64) -> Result<String, String> {
    conn.query_row(
        &format!(
            "SELECT rsduck_physical_type FROM rsduck_catalog.pg_type WHERE oid = {pg_type_oid}"
        ),
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("lookup DuckDB type for pg_type oid {pg_type_oid} failed: {e}"))
}


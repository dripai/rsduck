fn allocate_oid(conn: &Connection) -> Result<i64, String> {
    let oid: i64 = conn
        .query_row(
            "SELECT next_oid FROM rsduck_catalog.rs_oid_alloc WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read next oid failed: {e}"))?;
    conn.execute(
        "UPDATE rsduck_catalog.rs_oid_alloc SET next_oid = next_oid + 1, updated_at = CURRENT_TIMESTAMP WHERE id = 1",
        [],
    )
    .map_err(|e| format!("advance oid allocator failed: {e}"))?;
    Ok(oid)
}

fn catalog_epoch(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT catalog_epoch FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("read catalog epoch failed: {e}"))
}


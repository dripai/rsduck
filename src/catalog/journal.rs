use super::*;

pub(super) fn run_catalog_tx<T, F>(conn: &Connection, f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("begin catalog mutation failed: {e}"))?;
    match f() {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("commit catalog mutation failed: {e}"))?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

pub(super) fn insert_journal(
    conn: &Connection,
    mutation_type: &str,
    target_oid: i64,
    request: &str,
) -> Result<i64, String> {
    let journal_id = allocate_oid(conn)?;
    let next_epoch = catalog_epoch(conn)? + 1;
    conn.execute(
        &format!(
            "INSERT INTO rsduck_catalog.rs_catalog_journal(journal_id, catalog_epoch, mutation_type, target_oid, request_json, status, error_message, created_at, applied_at) \
             VALUES ({journal_id}, {next_epoch}, '{}', {target_oid}, '{}', 'pending', '', CURRENT_TIMESTAMP, NULL)",
            sql_string(mutation_type),
            sql_string(request)
        ),
        [],
    )
    .map_err(|e| format!("write catalog journal failed: {e}"))?;
    Ok(journal_id)
}

pub(super) fn finish_journal(conn: &Connection, journal_id: i64) -> Result<(), String> {
    let (mutation_type, target_oid): (String, i64) = conn
        .query_row(
            &format!(
                "SELECT mutation_type, target_oid \
                 FROM rsduck_catalog.rs_catalog_journal \
                 WHERE journal_id = {journal_id}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("read catalog journal audit fields failed: {e}"))?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_catalog_journal SET status = 'applied', applied_at = CURRENT_TIMESTAMP WHERE journal_id = {journal_id}"
        ),
        [],
    )
    .map_err(|e| format!("finish catalog journal failed: {e}"))?;
    conn.execute(
        "UPDATE rsduck_catalog.rs_catalog_version \
         SET catalog_epoch = catalog_epoch + 1, updated_at = CURRENT_TIMESTAMP \
         WHERE id = 1",
        [],
    )
    .map_err(|e| format!("increment catalog epoch failed: {e}"))?;
    refresh_catalog_checksum(conn)?;
    info!(
        target: "rsduck_audit",
        event = "catalog_mutation_applied",
        journal_id = journal_id,
        mutation_type = mutation_type.as_str(),
        target_oid = target_oid
    );
    Ok(())
}

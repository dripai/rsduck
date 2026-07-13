use super::*;

pub(crate) fn restore_or_initialize(
    conn: &Connection,
    snapshot_dir: Option<&str>,
    init_sql_path: &str,
) -> Result<(), String> {
    if let Some(path) = snapshot_dir {
        let t0 = Instant::now();
        info!("Restoring from snapshot dir: {}", path);
        prepare_snapshot_parquet_extension(conn, Path::new(path).parent())?;
        restore_snapshot_v2(conn, Path::new(path))?;
        info!("Snapshot restored in {:.2?}", t0.elapsed());
        info!(
            target: "rsduck_audit",
            event = "snapshot_restore",
            path = path
        );
        return Ok(());
    }

    crate::catalog::bootstrap_fresh(conn)?;

    let init_sql_path = init_sql_path.trim();
    if init_sql_path.is_empty() {
        info!("No snapshot dir found and init_sql is empty, starting empty in-memory DuckDB");
        crate::catalog::validate_after_start(conn)?;
        return Ok(());
    }

    let path = Path::new(init_sql_path);
    if !path.is_file() {
        return Err(format!("init_sql file not found: {init_sql_path}"));
    }

    let t0 = Instant::now();
    info!("Initializing DuckDB from init_sql: {}", init_sql_path);
    let sql = fs::read_to_string(path).map_err(|e| format!("read init_sql failed: {e}"))?;
    crate::catalog::execute_init_sql(conn, &sql)?;
    crate::catalog::validate_after_start(conn)?;
    info!("init_sql executed in {:.2?}", t0.elapsed());
    Ok(())
}

fn save_snapshot_blocking(
    conn: &Connection,
    snapshot_dir: &str,
    snapshot_prefix: &str,
) -> Result<String, String> {
    validate_snapshot_prefix(snapshot_prefix)?;
    std::fs::create_dir_all(snapshot_dir)
        .map_err(|e| format!("create snapshot dir failed: {e}"))?;

    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let final_path = Path::new(snapshot_dir).join(format!("{snapshot_prefix}_{ts}"));
    let tmp_path = Path::new(snapshot_dir).join(format!("{snapshot_prefix}_{ts}.tmp"));

    if final_path.exists() {
        return Err(format!(
            "snapshot target already exists: {}",
            final_path.display()
        ));
    }
    if tmp_path.exists() {
        return Err(format!(
            "snapshot temp dir already exists: {}",
            tmp_path.display()
        ));
    }

    prepare_snapshot_parquet_extension(conn, Some(Path::new(snapshot_dir)))?;
    let tmp_path_text = tmp_path.display().to_string();
    conn.execute_batch(&export_database_sql(&tmp_path_text))
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_path);
            format!("export snapshot failed: {e}")
        })?;
    write_snapshot_manifest(conn, &tmp_path, &final_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp_path);
        e
    })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp_path);
        format!("rename snapshot dir failed: {e}")
    })?;
    Ok(final_path.display().to_string())
}

fn prepare_snapshot_parquet_extension(
    conn: &Connection,
    base_dir: Option<&Path>,
) -> Result<(), String> {
    let extension_dir = match base_dir {
        Some(path) => path.join(".rsduck_duckdb_extensions"),
        None => std::env::temp_dir().join(".rsduck_duckdb_extensions"),
    };
    std::fs::create_dir_all(&extension_dir)
        .map_err(|e| format!("create DuckDB extension dir failed: {e}"))?;
    let extension_dir_text = extension_dir.display().to_string();
    conn.execute_batch(&format!(
        "SET extension_directory = '{}'; INSTALL parquet; LOAD parquet;",
        escape_sql_string(&extension_dir_text)
    ))
    .map_err(|e| format!("prepare parquet extension failed: {e}"))?;
    Ok(())
}

fn write_snapshot_manifest(
    conn: &Connection,
    tmp_path: &Path,
    final_path: &Path,
) -> Result<(), String> {
    let (catalog_epoch, catalog_checksum): (i64, String) = conn
        .query_row(
            "SELECT catalog_epoch, catalog_checksum \
             FROM rsduck_catalog.rs_catalog_version \
             WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("read snapshot catalog metadata failed: {e}"))?;
    let snapshot_name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| {
            format!(
                "snapshot final path has no file name: {}",
                final_path.display()
            )
        })?;
    let manifest = serde_json::json!({
        "manifest_version": 1,
        "snapshot_name": snapshot_name,
        "catalog_epoch": catalog_epoch,
        "catalog_checksum": catalog_checksum,
    });
    let payload = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| format!("serialize snapshot manifest failed: {e}"))?;
    fs::write(tmp_path.join(SNAPSHOT_MANIFEST_FILE), payload)
        .map_err(|e| format!("write snapshot manifest failed: {e}"))?;
    Ok(())
}

fn validate_snapshot_manifest(conn: &Connection, snapshot_path: &Path) -> Result<(), String> {
    let manifest_path = snapshot_path.join(SNAPSHOT_MANIFEST_FILE);
    let payload = fs::read(&manifest_path).map_err(|e| {
        format!(
            "read snapshot manifest failed: {}: {e}",
            manifest_path.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|e| format!("parse snapshot manifest failed: {e}"))?;
    let version = manifest
        .get("manifest_version")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| "snapshot manifest missing manifest_version".to_string())?;
    if version != 1 {
        return Err(format!("unsupported snapshot manifest version: {version}"));
    }

    let expected_name = snapshot_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| {
            format!(
                "snapshot path has no file name: {}",
                snapshot_path.display()
            )
        })?;
    let manifest_name = manifest
        .get("snapshot_name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "snapshot manifest missing snapshot_name".to_string())?;
    if manifest_name != expected_name {
        return Err(format!(
            "snapshot manifest name mismatch: expected={expected_name}, actual={manifest_name}"
        ));
    }

    let manifest_epoch = manifest
        .get("catalog_epoch")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| "snapshot manifest missing catalog_epoch".to_string())?;
    let manifest_checksum = manifest
        .get("catalog_checksum")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "snapshot manifest missing catalog_checksum".to_string())?;
    let (catalog_epoch, catalog_checksum): (i64, String) = conn
        .query_row(
            "SELECT catalog_epoch, catalog_checksum \
             FROM rsduck_catalog.rs_catalog_version \
             WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("read restored catalog metadata failed: {e}"))?;

    if manifest_epoch != catalog_epoch {
        return Err(format!(
            "snapshot manifest catalog_epoch mismatch: expected={manifest_epoch}, actual={catalog_epoch}"
        ));
    }
    if manifest_checksum != catalog_checksum {
        return Err(format!(
            "snapshot manifest catalog_checksum mismatch: expected={manifest_checksum}, actual={catalog_checksum}"
        ));
    }
    Ok(())
}

pub fn reset_admin_password_offline(
    snapshot_dir: &str,
    snapshot_prefix: &str,
    new_password: &str,
) -> Result<String, String> {
    validate_snapshot_prefix(snapshot_prefix)?;
    let snapshot = find_latest_snapshot_dir(snapshot_dir, snapshot_prefix)
        .ok_or_else(|| format!("no snapshot found in {snapshot_dir} with prefix {snapshot_prefix}"))?;
    let snapshot_path = PathBuf::from(&snapshot);
    let snapshot_name = snapshot_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .ok_or_else(|| format!("snapshot path has no file name: {}", snapshot_path.display()))?;
    if snapshot_name.ends_with(".tmp") {
        return Err(format!(
            "refuse to reset admin password from temp snapshot: {}",
            snapshot_path.display()
        ));
    }

    let conn =
        Connection::open_in_memory().map_err(|e| format!("open maintenance DuckDB failed: {e}"))?;
    prepare_snapshot_parquet_extension(&conn, snapshot_path.parent())?;
    conn.execute_batch(&import_database_sql(&snapshot))
        .map_err(|e| format!("import snapshot failed: {e}"))?;
    validate_snapshot_manifest(&conn, &snapshot_path)?;
    crate::catalog::validate_after_start(&conn)?;

    let sql = format!(
        "ALTER USER admin PASSWORD '{}'",
        escape_sql_string(new_password)
    );
    let affected = crate::catalog::execute_catalog_aware_write(&conn, &sql)?;
    if affected != Some(1) {
        return Err("admin password reset did not update exactly one user".into());
    }

    save_snapshot_blocking(&conn, snapshot_dir, snapshot_prefix)
}

pub fn find_latest_snapshot_dir(snapshot_dir: &str, snapshot_prefix: &str) -> Option<String> {
    let base = Path::new(snapshot_dir);
    if !base.exists() {
        return None;
    }

    let mut files: Vec<(chrono::NaiveDateTime, String)> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let ts = parse_snapshot_dir_timestamp(&name, snapshot_prefix)?;
            Some((ts, name))
        })
        .collect();

    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files
        .first()
        .map(|(_, name)| PathBuf::from(snapshot_dir).join(name).display().to_string())
}

pub fn parse_snapshot_dir_timestamp(
    file_name: &str,
    snapshot_prefix: &str,
) -> Option<chrono::NaiveDateTime> {
    let prefix = format!("{snapshot_prefix}_");
    let ts_part = file_name.strip_prefix(&prefix)?;
    if ts_part.ends_with(".tmp") || ts_part.contains('.') {
        return None;
    }

    chrono::NaiveDateTime::parse_from_str(ts_part, "%Y%m%d_%H%M%S").ok()
}

pub fn export_database_sql(snapshot_path: &str) -> String {
    format!(
        "EXPORT DATABASE '{}' (FORMAT parquet, COMPRESSION zstd)",
        escape_sql_string(snapshot_path)
    )
}

pub fn import_database_sql(snapshot_path: &str) -> String {
    format!("IMPORT DATABASE '{}'", escape_sql_string(snapshot_path))
}

pub fn validate_snapshot_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() {
        return Err("snapshot prefix is empty".into());
    }
    if !prefix
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "snapshot prefix contains unsupported characters: {prefix}"
        ));
    }
    Ok(())
}

fn escape_sql_string(input: &str) -> String {
    input.replace('\'', "''")
}

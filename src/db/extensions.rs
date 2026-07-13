use super::*;

const VSS_EXTENSION: &str = "vss";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStatus {
    pub name: String,
    pub installed: bool,
    pub loaded: bool,
    pub version: String,
    pub install_mode: String,
    pub installed_from: String,
}

pub(super) fn prepare_configured_extensions(
    conn: &Connection,
    config: &DbConfig,
) -> Result<(), String> {
    if !config.vss_enabled {
        return Ok(());
    }
    let extension_dir = prepare_extension_directory(&config.extension_dir)?;
    set_extension_directory(conn, &extension_dir)?;
    if conn.execute_batch("LOAD vss;").is_err() {
        conn.execute_batch("INSTALL vss; LOAD vss;")
            .map_err(|e| format!("prepare required DuckDB VSS extension failed: {e}"))?;
    }
    let status = extension_status(conn, VSS_EXTENSION)?;
    if !status.installed || !status.loaded {
        return Err(format!(
            "required DuckDB VSS extension is unavailable: installed={}, loaded={}",
            status.installed, status.loaded
        ));
    }
    info!(
        extension = status.name,
        version = status.version,
        install_mode = status.install_mode,
        installed_from = status.installed_from,
        "DuckDB extension ready"
    );
    Ok(())
}

pub fn install_vss_extension(extension_dir: &str) -> Result<ExtensionStatus, String> {
    let conn = Connection::open_in_memory()
        .map_err(|e| format!("open DuckDB for VSS extension preparation failed: {e}"))?;
    let config = DbConfig {
        extension_dir: extension_dir.to_string(),
        vss_enabled: true,
        ..DbConfig::default()
    };
    prepare_configured_extensions(&conn, &config)?;
    extension_status(&conn, VSS_EXTENSION)
}

pub(super) fn load_configured_extensions(
    conn: &Connection,
    config: &DbConfig,
) -> Result<(), String> {
    if !config.vss_enabled {
        return Ok(());
    }
    let extension_dir = prepare_extension_directory(&config.extension_dir)?;
    set_extension_directory(conn, &extension_dir)?;
    conn.execute_batch("LOAD vss;")
        .map_err(|e| format!("load required DuckDB VSS extension failed: {e}"))?;
    let status = extension_status(conn, VSS_EXTENSION)?;
    if !status.loaded {
        return Err("required DuckDB VSS extension did not load".into());
    }
    Ok(())
}

pub fn extension_status(conn: &Connection, name: &str) -> Result<ExtensionStatus, String> {
    if name != VSS_EXTENSION {
        return Err(format!("DuckDB extension is not allowed by rsduck: {name}"));
    }
    conn.query_row(
        "SELECT extension_name, installed, loaded,
                COALESCE(extension_version, ''), COALESCE(install_mode, ''),
                COALESCE(installed_from, '')
         FROM duckdb_extensions() WHERE extension_name = 'vss'",
        [],
        |row| {
            Ok(ExtensionStatus {
                name: row.get(0)?,
                installed: row.get(1)?,
                loaded: row.get(2)?,
                version: row.get(3)?,
                install_mode: row.get(4)?,
                installed_from: row.get(5)?,
            })
        },
    )
    .map_err(|e| format!("read DuckDB extension status failed: {e}"))
}

fn prepare_extension_directory(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path.trim());
    if path.as_os_str().is_empty() {
        return Err("db.extension_dir cannot be empty when VSS is enabled".into());
    }
    std::fs::create_dir_all(path)
        .map_err(|e| format!("create DuckDB extension directory failed: {e}"))?;
    std::fs::canonicalize(path)
        .map_err(|e| format!("resolve DuckDB extension directory failed: {e}"))
}

fn set_extension_directory(conn: &Connection, path: &Path) -> Result<(), String> {
    let value = path.display().to_string().replace('\'', "''");
    conn.execute_batch(&format!("SET extension_directory = '{value}';"))
        .map_err(|e| format!("set DuckDB extension directory failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_status_rejects_non_allowlisted_extension() {
        let conn = Connection::open_in_memory().unwrap();
        let error = extension_status(&conn, "httpfs").unwrap_err();
        assert_eq!(error, "DuckDB extension is not allowed by rsduck: httpfs");
    }

    #[test]
    fn disabled_vss_does_not_create_extension_directory() {
        let conn = Connection::open_in_memory().unwrap();
        let path = std::env::temp_dir().join(format!(
            "rsduck_disabled_vss_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        let config = DbConfig {
            vss_enabled: false,
            extension_dir: path.display().to_string(),
            ..DbConfig::default()
        };
        prepare_configured_extensions(&conn, &config).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn configured_vss_missing_file_fails_without_implicit_install() {
        let conn = Connection::open_in_memory().unwrap();
        let path = std::env::temp_dir().join(format!(
            "rsduck_missing_vss_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        let config = DbConfig {
            vss_enabled: true,
            extension_dir: path.display().to_string(),
            ..DbConfig::default()
        };
        let error = load_configured_extensions(&conn, &config).unwrap_err();
        assert!(error.contains("load required DuckDB VSS extension failed"));
        assert!(path.exists());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    #[ignore = "downloads the platform-specific DuckDB VSS extension"]
    fn configured_vss_installs_loads_and_builds_hnsw() {
        let conn = Connection::open_in_memory().unwrap();
        let path = std::env::temp_dir().join(format!(
            "rsduck_vss_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        let config = DbConfig {
            vss_enabled: true,
            extension_dir: path.display().to_string(),
            ..DbConfig::default()
        };
        prepare_configured_extensions(&conn, &config).unwrap();
        let status = extension_status(&conn, VSS_EXTENSION).unwrap();
        assert!(status.installed);
        assert!(status.loaded);

        conn.execute_batch(
            "CREATE TABLE vector_smoke(id BIGINT, embedding FLOAT[3]);
             INSERT INTO vector_smoke VALUES (1, [0.12, 0.35, 0.78]::FLOAT[3]);
             CREATE INDEX vector_smoke_hnsw ON vector_smoke USING HNSW (embedding)
             WITH (metric = 'cosine');",
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM duckdb_indexes() WHERE index_name = 'vector_smoke_hnsw'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        drop(conn);
        let _ = std::fs::remove_dir_all(path);
    }
}

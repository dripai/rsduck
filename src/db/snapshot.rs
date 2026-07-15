use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SNAPSHOT_FORMAT_VERSION: i64 = crate::catalog::SNAPSHOT_FORMAT_VERSION;
const CATALOG_SNAPSHOT_FILE: &str = "catalog.duckdb";
const CATALOG_TABLES: &[&str] = &[
    "rs_catalog_version",
    "rs_oid_alloc",
    "rs_catalog_journal",
    "rs_schema",
    "rs_type",
    "rs_relation",
    "rs_column",
    "rs_column_default",
    "rs_constraint",
    "rs_index",
    "rs_vector_index",
    "rs_dependency",
    "rs_comment",
    "rs_relation_ext",
    "rs_partition",
    "rs_user",
    "rs_role",
    "rs_user_role",
    "rs_privilege",
];

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotManifest {
    snapshot_format_version: i64,
    snapshot_name: String,
    created_at: String,
    catalog_epoch: i64,
    catalog_checksum: String,
    rsduck_version: String,
    tables: Vec<SnapshotTable>,
    partitions: Vec<SnapshotPartition>,
    views: Vec<SnapshotView>,
    macros: Vec<SnapshotMacro>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotTable {
    relation_id: i64,
    schema: String,
    relation: String,
    file: String,
    row_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotPartition {
    parent_relation_id: i64,
    child_relation_id: i64,
    partition_value: String,
    status: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotView {
    relation_id: i64,
    schema: String,
    name: String,
    ddl: String,
    checksum: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotMacro {
    schema: String,
    name: String,
    macro_type: String,
    parameters: String,
    ddl: String,
    checksum: String,
}

pub(crate) fn save_snapshot_blocking(
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
    if final_path.exists() || tmp_path.exists() {
        return Err(format!(
            "snapshot target already exists: {}",
            final_path.display()
        ));
    }

    let result: Result<String, String> = (|| {
        std::fs::create_dir_all(tmp_path.join("data"))
            .map_err(|e| format!("create snapshot data dir failed: {e}"))?;
        prepare_snapshot_parquet_extension(conn, Some(Path::new(snapshot_dir)))?;
        let (catalog_epoch, catalog_checksum) = catalog_snapshot_metadata(conn)?;
        export_catalog_database(conn, &tmp_path.join(CATALOG_SNAPSHOT_FILE))?;
        let tables = export_snapshot_tables(conn, &tmp_path)?;
        let partitions = snapshot_partitions(conn)?;
        let views = snapshot_views(conn)?;
        let macros = snapshot_macros(conn)?;
        let (final_epoch, final_checksum) = catalog_snapshot_metadata(conn)?;
        if final_epoch != catalog_epoch || final_checksum != catalog_checksum {
            return Err("catalog changed while snapshot was being exported".into());
        }
        write_snapshot_manifest(
            &tmp_path,
            &final_path,
            catalog_epoch,
            catalog_checksum,
            tables,
            partitions,
            views,
            macros,
        )?;
        std::fs::rename(&tmp_path, &final_path)
            .map_err(|e| format!("rename snapshot dir failed: {e}"))?;
        Ok(final_path.display().to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp_path);
    }
    result
}

fn catalog_snapshot_metadata(conn: &Connection) -> Result<(i64, String), String> {
    conn.query_row(
        "SELECT catalog_epoch, catalog_checksum FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(|e| format!("read snapshot catalog metadata failed: {e}"))
}

fn export_catalog_database(conn: &Connection, path: &Path) -> Result<(), String> {
    let path_text = path.display().to_string();
    conn.execute_batch(&format!(
        "ATTACH '{}' AS rsduck_snapshot_catalog;",
        escape_sql_string(&path_text)
    ))
    .map_err(|e| format!("create snapshot catalog database failed: {e}"))?;
    let result: Result<(), String> = (|| {
        for table in CATALOG_TABLES {
            conn.execute_batch(&format!(
                "CREATE TABLE rsduck_snapshot_catalog.{table} AS SELECT * FROM rsduck_catalog.{table};"
            ))
            .map_err(|e| format!("copy catalog table {table} failed: {e}"))?;
        }
        Ok(())
    })();
    let detach = conn.execute_batch("DETACH rsduck_snapshot_catalog;");
    result?;
    detach.map_err(|e| format!("close snapshot catalog database failed: {e}"))?;
    Ok(())
}

fn export_snapshot_tables(
    conn: &Connection,
    tmp_path: &Path,
) -> Result<Vec<SnapshotTable>, String> {
    let relations = snapshot_data_relations(conn)?;
    let mut tables = Vec::new();
    for (relation_id, schema, relation) in relations {
        let file = format!("data/{relation_id}.parquet");
        let file_path = tmp_path.join(&file);
        conn.execute_batch(&format!(
            "COPY {} TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            quote_qualified(&schema, &relation),
            escape_sql_string(&file_path.display().to_string())
        ))
        .map_err(|e| format!("export snapshot data {schema}.{relation} failed: {e}"))?;
        let row_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {}",
                    quote_qualified(&schema, &relation)
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("count snapshot data {schema}.{relation} failed: {e}"))?;
        tables.push(SnapshotTable {
            relation_id,
            schema,
            relation,
            file,
            row_count,
        });
    }
    Ok(tables)
}

fn snapshot_data_relations(conn: &Connection) -> Result<Vec<(i64, String, String)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, n.nspname, c.relname
             FROM rsduck_catalog.rs_relation c
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace
             JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
             WHERE c.status = 'active' AND c.relkind = 'r'
               AND ext.visibility IN ('user', 'internal')
               AND n.nspname NOT IN ('rsduck_catalog')
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare snapshot relation lookup failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| format!("query snapshot relation lookup failed: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot relation lookup failed: {e}"))
}

fn snapshot_partitions(conn: &Connection) -> Result<Vec<SnapshotPartition>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT parent_relid, child_relid, partition_value, status
             FROM rsduck_catalog.rs_partition ORDER BY parent_relid, child_relid",
        )
        .map_err(|e| format!("prepare snapshot partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query snapshot partition lookup failed: {e}"))?;
    let mut partitions = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read snapshot partition lookup failed: {e}"))?
    {
        partitions.push(SnapshotPartition {
            parent_relation_id: row
                .get(0)
                .map_err(|e| format!("read partition parent failed: {e}"))?,
            child_relation_id: row
                .get(1)
                .map_err(|e| format!("read partition child failed: {e}"))?,
            partition_value: row
                .get(2)
                .map_err(|e| format!("read partition value failed: {e}"))?,
            status: row
                .get(3)
                .map_err(|e| format!("read partition status failed: {e}"))?,
        });
    }
    Ok(partitions)
}

fn snapshot_views(conn: &Connection) -> Result<Vec<SnapshotView>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.oid, v.schema_name, v.view_name, v.sql
             FROM duckdb_views() v
             JOIN rsduck_catalog.rs_schema n ON lower(n.nspname) = lower(v.schema_name)
             JOIN rsduck_catalog.rs_relation c
               ON c.relnamespace = n.oid AND lower(c.relname) = lower(v.view_name)
             JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
             WHERE v.internal = FALSE
               AND c.status = 'active'
               AND ext.visibility = 'user'
               AND c.relkind IN ('v', 'p')
             ORDER BY c.oid",
        )
        .map_err(|e| format!("prepare snapshot view lookup failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| format!("query snapshot view lookup failed: {e}"))?;
    rows.map(|row| {
        let (relation_id, schema, name, ddl) =
            row.map_err(|e| format!("read snapshot view failed: {e}"))?;
        Ok(SnapshotView {
            relation_id,
            schema,
            name,
            checksum: snapshot_ddl_checksum(&ddl),
            ddl,
        })
    })
    .collect()
}

fn snapshot_macros(conn: &Connection) -> Result<Vec<SnapshotMacro>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT schema_name, function_name, function_type,
                    COALESCE(array_to_string(parameters, ', '), ''),
                    macro_definition
             FROM duckdb_functions()
             WHERE function_type IN ('macro', 'table_macro')
               AND database_name = current_database()
               AND schema_name NOT IN ('information_schema', 'pg_catalog', 'rsduck_catalog', 'rsduck_internal')
             ORDER BY schema_name, function_name",
        )
        .map_err(|e| format!("prepare snapshot macro lookup failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| format!("query snapshot macro lookup failed: {e}"))?;
    rows.map(|row| {
        let (schema, name, macro_type, parameters, definition) =
            row.map_err(|e| format!("read snapshot macro failed: {e}"))?;
        let ddl = if macro_type == "table_macro" {
            format!(
                "CREATE MACRO {}({parameters}) AS TABLE {definition}",
                quote_qualified(&schema, &name)
            )
        } else {
            format!(
                "CREATE MACRO {}({parameters}) AS {definition}",
                quote_qualified(&schema, &name)
            )
        };
        Ok(SnapshotMacro {
            schema,
            name,
            macro_type,
            parameters,
            checksum: snapshot_ddl_checksum(&ddl),
            ddl,
        })
    })
    .collect()
}

fn write_snapshot_manifest(
    tmp_path: &Path,
    final_path: &Path,
    catalog_epoch: i64,
    catalog_checksum: String,
    tables: Vec<SnapshotTable>,
    partitions: Vec<SnapshotPartition>,
    views: Vec<SnapshotView>,
    macros: Vec<SnapshotMacro>,
) -> Result<(), String> {
    let snapshot_name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| format!("snapshot path has no file name: {}", final_path.display()))?;
    let manifest = SnapshotManifest {
        snapshot_format_version: SNAPSHOT_FORMAT_VERSION,
        snapshot_name,
        created_at: chrono::Local::now().to_rfc3339(),
        catalog_epoch,
        catalog_checksum,
        rsduck_version: env!("CARGO_PKG_VERSION").to_string(),
        tables,
        partitions,
        views,
        macros,
    };
    let payload = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| format!("serialize snapshot manifest failed: {e}"))?;
    fs::write(tmp_path.join(SNAPSHOT_MANIFEST_FILE), payload)
        .map_err(|e| format!("write snapshot manifest failed: {e}"))
}

pub(super) fn restore_snapshot_v2(conn: &Connection, snapshot_path: &Path) -> Result<(), String> {
    let manifest = read_snapshot_manifest(snapshot_path)?;
    let catalog_path = snapshot_path.join(CATALOG_SNAPSHOT_FILE);
    if !catalog_path.is_file() {
        return Err(format!(
            "snapshot catalog file is missing: {}",
            catalog_path.display()
        ));
    }
    crate::catalog::create_catalog_storage(conn)?;
    import_catalog_database(conn, &catalog_path)?;
    validate_catalog_snapshot_format(conn)?;
    let (epoch, checksum) = catalog_snapshot_metadata(conn)?;
    if epoch != manifest.catalog_epoch || checksum != manifest.catalog_checksum {
        return Err("snapshot manifest catalog metadata does not match catalog.duckdb".into());
    }
    restore_snapshot_schemas(conn)?;
    for table in &manifest.tables {
        restore_snapshot_table(conn, snapshot_path, table)?;
    }
    restore_snapshot_indexes(conn)?;
    restore_snapshot_views(conn, &manifest.views)?;
    restore_snapshot_macros(conn, &manifest.macros)?;
    crate::catalog::refresh_catalog_checksum(conn)?;
    crate::catalog::validate_after_start(conn)
}

fn read_snapshot_manifest(snapshot_path: &Path) -> Result<SnapshotManifest, String> {
    let manifest_path = snapshot_path.join(SNAPSHOT_MANIFEST_FILE);
    let payload = fs::read(&manifest_path).map_err(|e| {
        format!(
            "read snapshot manifest failed: {}: {e}",
            manifest_path.display()
        )
    })?;
    let manifest: SnapshotManifest = serde_json::from_slice(&payload)
        .map_err(|e| format!("parse snapshot manifest failed: {e}"))?;
    if manifest.snapshot_format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(format!(
            "unsupported snapshot format version: {}",
            manifest.snapshot_format_version
        ));
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
    if manifest.snapshot_name != expected_name {
        return Err(format!(
            "snapshot manifest name mismatch: expected={expected_name}, actual={}",
            manifest.snapshot_name
        ));
    }
    Ok(manifest)
}

fn import_catalog_database(conn: &Connection, catalog_path: &Path) -> Result<(), String> {
    let path_text = catalog_path.display().to_string();
    conn.execute_batch(&format!(
        "ATTACH '{}' AS rsduck_snapshot_catalog (READ_ONLY);",
        escape_sql_string(&path_text)
    ))
    .map_err(|e| format!("open snapshot catalog database failed: {e}"))?;
    let result: Result<(), String> = (|| {
        for table in CATALOG_TABLES {
            conn.execute_batch(&format!(
                "INSERT INTO rsduck_catalog.{table} SELECT * FROM rsduck_snapshot_catalog.{table};"
            ))
            .map_err(|e| format!("restore catalog table {table} failed: {e}"))?;
        }
        Ok(())
    })();
    let detach = conn.execute_batch("DETACH rsduck_snapshot_catalog;");
    result?;
    detach.map_err(|e| format!("close snapshot catalog database failed: {e}"))?;
    Ok(())
}

fn validate_catalog_snapshot_format(conn: &Connection) -> Result<(), String> {
    let version: i64 = conn
        .query_row(
            "SELECT snapshot_format_version FROM rsduck_catalog.rs_catalog_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("read snapshot catalog format version failed: {e}"))?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(format!(
            "unsupported catalog snapshot format version: {version}"
        ));
    }
    Ok(())
}

fn restore_snapshot_schemas(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT nspname FROM rsduck_catalog.rs_schema
             WHERE nspname NOT IN ('rsduck_catalog', 'rsduck_internal', 'information_schema', 'pg_catalog')
             ORDER BY oid",
        )
        .map_err(|e| format!("prepare snapshot schema restore failed: {e}"))?;
    let schemas = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query snapshot schema restore failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot schema restore failed: {e}"))?;
    drop(stmt);
    for schema in schemas {
        conn.execute_batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(&schema)
        ))
        .map_err(|e| format!("create snapshot schema {schema} failed: {e}"))?;
    }
    Ok(())
}

fn restore_snapshot_table(
    conn: &Connection,
    snapshot_path: &Path,
    table: &SnapshotTable,
) -> Result<(), String> {
    let file_path = snapshot_path.join(&table.file);
    if !file_path.is_file() {
        crate::catalog::mark_relation_unavailable(
            conn,
            table.relation_id,
            &format!("snapshot data file is missing: {}", file_path.display()),
        )?;
        return Ok(());
    }
    let qualified = quote_qualified(&table.schema, &table.relation);
    repair_legacy_decimal_modifiers_from_parquet(conn, table.relation_id, &file_path)?;
    let columns = snapshot_table_columns(conn, table.relation_id)?;
    conn.execute_batch(&format!(
        "CREATE TABLE {qualified} ({});",
        columns.join(", ")
    ))
    .map_err(|e| {
        format!(
            "create snapshot table {}.{} from catalog failed: {e}",
            table.schema, table.relation
        )
    })?;
    conn.execute_batch(&format!(
        "INSERT INTO {qualified} SELECT * FROM read_parquet('{}');",
        escape_sql_string(&file_path.display().to_string())
    ))
    .map_err(|e| {
        format!(
            "restore snapshot data {}.{} failed: {e}",
            table.schema, table.relation
        )
    })?;
    let actual_rows: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {qualified}"), [], |row| {
            row.get(0)
        })
        .map_err(|e| {
            format!(
                "count restored data {}.{} failed: {e}",
                table.schema, table.relation
            )
        })?;
    if actual_rows != table.row_count {
        return Err(format!(
            "snapshot data row count mismatch for {}.{}: expected={}, actual={actual_rows}",
            table.schema, table.relation, table.row_count
        ));
    }
    Ok(())
}

fn repair_legacy_decimal_modifiers_from_parquet(
    conn: &Connection,
    relation_id: i64,
    file_path: &Path,
) -> Result<(), String> {
    let mut catalog_stmt = conn
        .prepare(&format!(
            "SELECT attname, attnum, atttypid, atttypmod \
             FROM rsduck_catalog.rs_column \
             WHERE attrelid = {relation_id} AND NOT attisdropped \
             ORDER BY attnum"
        ))
        .map_err(|e| format!("prepare snapshot DECIMAL modifier lookup failed: {e}"))?;
    let catalog_columns = catalog_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i32>(3)?,
            ))
        })
        .map_err(|e| format!("query snapshot DECIMAL modifiers failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot DECIMAL modifiers failed: {e}"))?;
    drop(catalog_stmt);

    if !catalog_columns
        .iter()
        .any(|(_, _, type_id, type_modifier)| {
            *type_id == crate::catalog::TYPE_NUMERIC && *type_modifier == -1
        })
    {
        return Ok(());
    }

    let mut parquet_stmt = conn
        .prepare(&format!(
            "DESCRIBE SELECT * FROM read_parquet('{}')",
            escape_sql_string(&file_path.display().to_string())
        ))
        .map_err(|e| format!("prepare snapshot Parquet schema inspection failed: {e}"))?;
    let parquet_columns = parquet_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query snapshot Parquet schema failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot Parquet schema failed: {e}"))?;
    drop(parquet_stmt);

    if catalog_columns.len() != parquet_columns.len() {
        return Err(format!(
            "snapshot Parquet column count mismatch for relation {relation_id}: catalog={}, parquet={}",
            catalog_columns.len(),
            parquet_columns.len()
        ));
    }
    for ((catalog_name, attnum, type_id, type_modifier), (parquet_name, parquet_type)) in
        catalog_columns.iter().zip(parquet_columns.iter())
    {
        if !catalog_name.eq_ignore_ascii_case(parquet_name) {
            return Err(format!(
                "snapshot Parquet column mismatch for relation {relation_id}: catalog={catalog_name}, parquet={parquet_name}"
            ));
        }
        if *type_id != crate::catalog::TYPE_NUMERIC {
            continue;
        }
        let parquet_modifier = crate::catalog::type_modifier_for_duckdb_type(parquet_type);
        if parquet_modifier < 0 {
            return Err(format!(
                "snapshot DECIMAL column has incompatible Parquet type: relation={relation_id}, column={catalog_name}, parquet_type={parquet_type}"
            ));
        }
        if *type_modifier != -1 {
            let catalog_type = crate::catalog::duckdb_type_with_modifier("DECIMAL", *type_modifier);
            if !catalog_type.eq_ignore_ascii_case(parquet_type) {
                return Err(format!(
                    "snapshot DECIMAL type mismatch: relation={relation_id}, column={catalog_name}, catalog={catalog_type}, parquet={parquet_type}"
                ));
            }
            continue;
        }
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_column SET atttypmod = {parquet_modifier} \
                 WHERE attrelid = {relation_id} AND attnum = {attnum} AND atttypmod = -1"
            ),
            [],
        )
        .map_err(|e| format!("repair legacy snapshot DECIMAL modifier failed: {e}"))?;
        tracing::warn!(
            relation_id,
            column = catalog_name.as_str(),
            parquet_type = parquet_type.as_str(),
            "repaired legacy snapshot DECIMAL modifier from Parquet schema"
        );
    }
    Ok(())
}

fn snapshot_table_columns(conn: &Connection, relation_id: i64) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT a.attname, t.rsduck_physical_type, a.atttypmod, a.attnotnull
             FROM rsduck_catalog.rs_column a
             JOIN rsduck_catalog.rs_type t ON t.oid = a.atttypid
             WHERE a.attrelid = {relation_id} AND NOT a.attisdropped
             ORDER BY a.attnum"
        ))
        .map_err(|e| format!("prepare snapshot table columns failed: {e}"))?;
    let columns = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i32>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .map_err(|e| format!("query snapshot table columns failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot table columns failed: {e}"))?;
    if columns.is_empty() {
        return Err(format!(
            "snapshot table relation {relation_id} has no catalog columns"
        ));
    }
    columns
        .into_iter()
        .map(|(name, physical_type, type_modifier, not_null)| {
            if !crate::catalog::is_valid_type_modifier_for_duckdb_type(
                &physical_type,
                type_modifier,
            ) {
                return Err(format!(
                    "snapshot DECIMAL column is missing or has invalid type modifier: relation={relation_id}, column={name}, atttypmod={type_modifier}"
                ));
            }
            Ok(format!(
                "{} {}{}",
                quote_ident(&name),
                crate::catalog::duckdb_type_with_modifier(&physical_type, type_modifier),
                if not_null { " NOT NULL" } else { "" }
            ))
        })
        .collect()
}

fn restore_snapshot_views(conn: &Connection, views: &[SnapshotView]) -> Result<(), String> {
    for view in views {
        validate_snapshot_ddl(&view.ddl, &view.checksum, "view", &view.schema, &view.name)?;
        conn.execute_batch(&view.ddl).map_err(|e| {
            format!(
                "restore snapshot view {}.{} failed: {e}",
                view.schema, view.name
            )
        })?;
    }
    Ok(())
}

fn restore_snapshot_macros(conn: &Connection, macros: &[SnapshotMacro]) -> Result<(), String> {
    for macro_object in macros {
        validate_snapshot_ddl(
            &macro_object.ddl,
            &macro_object.checksum,
            "macro",
            &macro_object.schema,
            &macro_object.name,
        )?;
        conn.execute_batch(&macro_object.ddl).map_err(|e| {
            format!(
                "restore snapshot macro {}.{} failed: {e}",
                macro_object.schema, macro_object.name
            )
        })?;
    }
    Ok(())
}

fn validate_snapshot_ddl(
    ddl: &str,
    expected_checksum: &str,
    object_type: &str,
    schema: &str,
    name: &str,
) -> Result<(), String> {
    if snapshot_ddl_checksum(ddl) != expected_checksum {
        return Err(format!(
            "snapshot {object_type} DDL checksum mismatch for {schema}.{name}"
        ));
    }
    Ok(())
}

fn snapshot_ddl_checksum(ddl: &str) -> String {
    format!("{:x}", Sha256::digest(ddl.as_bytes()))
}

fn restore_snapshot_indexes(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT ix.relname, i.indrelid, i.indisunique, i.indkey, tc.relkind, tn.nspname, tc.relname
             FROM rsduck_catalog.rs_index i
             JOIN rsduck_catalog.rs_relation ix ON ix.oid = i.indexrelid
             JOIN rsduck_catalog.rs_relation tc ON tc.oid = i.indrelid
             JOIN rsduck_catalog.rs_schema tn ON tn.oid = tc.relnamespace
             WHERE ix.status = 'active' AND tc.status = 'active'
               AND NOT EXISTS (SELECT 1 FROM rsduck_catalog.rs_vector_index v WHERE v.indexrelid = i.indexrelid)
             ORDER BY i.indexrelid",
        )
        .map_err(|e| format!("prepare snapshot index restore failed: {e}"))?;
    let indexes = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|e| format!("query snapshot index restore failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot index restore failed: {e}"))?;
    drop(stmt);
    for (index_name, relation_id, unique, indkey, relkind, schema, relation) in indexes {
        let columns = index_columns(conn, relation_id, &indkey)?;
        let unique = if unique { "UNIQUE " } else { "" };
        if relkind == "p" {
            restore_partition_indexes(conn, &index_name, unique, &columns, relation_id)?;
        } else {
            conn.execute_batch(&format!(
                "CREATE {unique}INDEX {} ON {} ({})",
                quote_ident(&index_name),
                quote_qualified(&schema, &relation),
                columns
            ))
            .map_err(|e| format!("restore snapshot index {schema}.{index_name} failed: {e}"))?;
        }
    }
    restore_snapshot_vector_indexes(conn)
}

fn restore_snapshot_vector_indexes(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "SELECT v.indexrelid, v.vector_space, tn.nspname, tc.relname, a.attname, ix.relname,
                    v.embedding_model, v.model_version, v.metric, v.m, v.m0,
                    v.ef_construction, v.default_ef_search
             FROM rsduck_catalog.rs_vector_index v
             JOIN rsduck_catalog.rs_index i ON i.indexrelid = v.indexrelid
             JOIN rsduck_catalog.rs_relation ix ON ix.oid = v.indexrelid
             JOIN rsduck_catalog.rs_relation tc ON tc.oid = i.indrelid
             JOIN rsduck_catalog.rs_schema tn ON tn.oid = tc.relnamespace
             JOIN rsduck_catalog.rs_column a ON a.attrelid = tc.oid AND CAST(a.attnum AS VARCHAR) = i.indkey
             WHERE ix.status = 'active' AND tc.status = 'active'
             ORDER BY v.indexrelid",
        )
        .map_err(|e| format!("prepare snapshot vector index restore failed: {e}"))?;
    let indexes = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                crate::catalog::VectorIndexCreateRequest {
                    vector_space: row.get(1)?,
                    schema: row.get(2)?,
                    table: row.get(3)?,
                    column: row.get(4)?,
                    index_name: row.get(5)?,
                    embedding_model: row.get(6)?,
                    model_version: row.get(7)?,
                    metric: row.get(8)?,
                    m: row.get(9)?,
                    m0: row.get(10)?,
                    ef_construction: row.get(11)?,
                    default_ef_search: row.get(12)?,
                },
            ))
        })
        .map_err(|e| format!("query snapshot vector indexes failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot vector indexes failed: {e}"))?;
    drop(stmt);

    for (index_oid, request) in indexes {
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_vector_index SET build_status = 'rebuilding', updated_at = CURRENT_TIMESTAMP, error_message = '' WHERE indexrelid = {index_oid}"
            ),
            [],
        )
        .map_err(|e| format!("mark snapshot vector index rebuilding failed: {e}"))?;
        let create_sql = crate::catalog::vector_index_create_sql(&request);
        if let Err(error) = conn.execute_batch(&create_sql) {
            let reason = format!("restore HNSW index failed: {error}");
            conn.execute(
                &format!(
                    "UPDATE rsduck_catalog.rs_vector_index SET build_status = 'failed', updated_at = CURRENT_TIMESTAMP, error_message = '{}' WHERE indexrelid = {index_oid}",
                    escape_sql_string(&reason)
                ),
                [],
            )
            .map_err(|e| format!("record snapshot vector index failure failed: {e}"))?;
            return Err(reason);
        }
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
            .map_err(|e| format!("count restored vector rows failed: {e}"))?;
        conn.execute(
            &format!(
                "UPDATE rsduck_catalog.rs_vector_index
                 SET build_status = 'active', generation = generation + 1, vector_count = {vector_count}, built_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP, error_message = ''
                 WHERE indexrelid = {index_oid}"
            ),
            [],
        )
        .map_err(|e| format!("mark restored vector index active failed: {e}"))?;
    }
    Ok(())
}

fn index_columns(conn: &Connection, relation_id: i64, indkey: &str) -> Result<String, String> {
    let mut columns = Vec::new();
    for attnum in indkey.split(',').filter(|value| !value.trim().is_empty()) {
        let attnum = attnum
            .trim()
            .parse::<i32>()
            .map_err(|_| format!("invalid snapshot index key: {indkey}"))?;
        let name: String = conn
            .query_row(
                &format!(
                    "SELECT attname FROM rsduck_catalog.rs_column WHERE attrelid = {relation_id} AND attnum = {attnum}"
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("read snapshot index column failed: {e}"))?;
        columns.push(quote_ident(&name));
    }
    if columns.is_empty() {
        return Err("snapshot index has no columns".into());
    }
    Ok(columns.join(", "))
}

fn restore_partition_indexes(
    conn: &Connection,
    index_name: &str,
    unique: &str,
    columns: &str,
    parent_relation_id: i64,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.relname FROM rsduck_catalog.rs_partition p
             JOIN rsduck_catalog.rs_relation c ON c.oid = p.child_relid
             WHERE p.parent_relid = {parent_relation_id} AND p.status = 'active'"
        ))
        .map_err(|e| format!("prepare snapshot partition index restore failed: {e}"))?;
    let children = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query snapshot partition index restore failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read snapshot partition index restore failed: {e}"))?;
    drop(stmt);
    for child in children {
        let child_index = format!("{child}__{index_name}");
        conn.execute_batch(&format!(
            "CREATE {unique}INDEX {} ON {} ({columns})",
            quote_ident(&child_index),
            quote_qualified("rsduck_internal", &child)
        ))
        .map_err(|e| format!("restore snapshot partition index {child_index} failed: {e}"))?;
    }
    Ok(())
}

pub(crate) fn prepare_snapshot_parquet_extension(
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

pub fn reset_admin_password_offline(
    snapshot_dir: &str,
    snapshot_prefix: &str,
    new_password: &str,
) -> Result<String, String> {
    validate_snapshot_prefix(snapshot_prefix)?;
    let snapshot = find_latest_snapshot_dir(snapshot_dir, snapshot_prefix).ok_or_else(|| {
        format!("no Snapshot v2 found in {snapshot_dir} with prefix {snapshot_prefix}")
    })?;
    let conn =
        Connection::open_in_memory().map_err(|e| format!("open maintenance DuckDB failed: {e}"))?;
    prepare_snapshot_parquet_extension(&conn, Path::new(&snapshot).parent())?;
    restore_snapshot_v2(&conn, Path::new(&snapshot))?;
    let sql = format!(
        "ALTER USER admin PASSWORD '{}'",
        escape_sql_string(new_password)
    );
    if crate::catalog::execute_catalog_aware_write(&conn, &sql)? != Some(1) {
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
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let timestamp = parse_snapshot_dir_timestamp(&name, snapshot_prefix)?;
            let path = entry.path();
            read_snapshot_manifest(&path).ok()?;
            Some((timestamp, name))
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

pub(super) fn escape_sql_string(input: &str) -> String {
    input.replace('\'', "''")
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('\"', "\"\""))
}

fn quote_qualified(schema: &str, relation: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(relation))
}

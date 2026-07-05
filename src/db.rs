use crate::config::DbConfig;
use crate::sql_route::{route_sql, SqlRoute};
use duckdb::{types::ValueRef, Connection};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use tokio::sync::oneshot;
use tracing::{error, info};

static DB_ENGINE: OnceLock<DbEngine> = OnceLock::new();
const SNAPSHOT_MANIFEST_FILE: &str = "rsduck_snapshot_manifest.json";

#[derive(Debug, Clone)]
pub enum SqlResult {
    Query {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Execute {
        command: String,
        affected_rows: usize,
    },
}

enum SqlCommand {
    Run {
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
        resp: oneshot::Sender<Result<SqlResult, String>>,
    },
    Authenticate {
        username: String,
        password: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    PrivilegeFunction {
        username: String,
        sql: String,
        resp: oneshot::Sender<Result<SqlResult, String>>,
    },
    Shutdown,
}

enum SnapshotCommand {
    Save {
        username: Option<String>,
        dir: String,
        prefix: String,
        resp: oneshot::Sender<Result<String, String>>,
    },
    Shutdown,
}

struct DbEngine {
    read_txs: Vec<SyncSender<SqlCommand>>,
    write_tx: SyncSender<SqlCommand>,
    snapshot_tx: SyncSender<SnapshotCommand>,
    next_read: AtomicUsize,
    _base_conn: Mutex<Connection>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

pub fn init_db(snapshot_dir: Option<&str>, cfg: &DbConfig) {
    let base_conn = Connection::open_in_memory().expect("open in-memory duckdb failed");
    restore_or_initialize(&base_conn, snapshot_dir, &cfg.init_sql)
        .unwrap_or_else(|e| panic!("initialize DuckDB failed: {e}"));

    let read_workers = cfg.read_workers.max(1);
    let max_result_rows = cfg.max_result_rows.max(1);
    let mut read_txs = Vec::with_capacity(read_workers);
    let mut workers = Vec::with_capacity(read_workers + 2);

    let write_conn = base_conn
        .try_clone()
        .expect("clone write connection failed");
    let (write_tx, write_rx) = sync_channel(cfg.write_queue_size.max(1));
    workers.push(spawn_sql_worker(
        "duckdb-write",
        write_conn,
        write_rx,
        max_result_rows,
    ));

    for idx in 0..read_workers {
        let read_conn = base_conn
            .try_clone()
            .unwrap_or_else(|e| panic!("clone read connection {idx} failed: {e}"));
        let (read_tx, read_rx) = sync_channel(cfg.read_queue_size.max(1));
        workers.push(spawn_sql_worker(
            format!("duckdb-read-{idx}"),
            read_conn,
            read_rx,
            max_result_rows,
        ));
        read_txs.push(read_tx);
    }

    let snapshot_conn = base_conn
        .try_clone()
        .expect("clone snapshot connection failed");
    let (snapshot_tx, snapshot_rx) = sync_channel(cfg.snapshot_queue_size.max(1));
    workers.push(spawn_snapshot_worker(
        "duckdb-snapshot",
        snapshot_conn,
        snapshot_rx,
    ));

    let engine = DbEngine {
        read_txs,
        write_tx,
        snapshot_tx,
        next_read: AtomicUsize::new(0),
        _base_conn: Mutex::new(base_conn),
        workers: Mutex::new(workers),
    };

    DB_ENGINE
        .set(engine)
        .unwrap_or_else(|_| panic!("db initialized twice"));
}

pub async fn execute_sql_as(username: String, sql: String) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim().to_string();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if crate::catalog::looks_like_privilege_function(&sql_trimmed) {
        return engine()
            .evaluate_privilege_function(username, sql_trimmed)
            .await;
    }

    let decision = route_sql(&sql_trimmed)?;
    match decision.route {
        SqlRoute::Read => {
            engine()
                .query(username, sql_trimmed, decision.route, decision.command)
                .await
        }
        SqlRoute::Write => {
            engine()
                .execute(username, sql_trimmed, decision.route, decision.command)
                .await
        }
    }
}

pub async fn save_snapshot(snapshot_dir: &str, snapshot_prefix: &str) -> Result<String, String> {
    engine()
        .save_snapshot(None, snapshot_dir.to_string(), snapshot_prefix.to_string())
        .await
}

pub async fn save_snapshot_as(
    username: String,
    snapshot_dir: &str,
    snapshot_prefix: &str,
) -> Result<String, String> {
    engine()
        .save_snapshot(
            Some(username),
            snapshot_dir.to_string(),
            snapshot_prefix.to_string(),
        )
        .await
}

pub async fn authenticate_user(username: String, password: String) -> Result<(), String> {
    engine().authenticate(username, password).await
}

pub async fn run_partition_maintenance() -> Result<SqlResult, String> {
    engine()
        .execute(
            "admin".to_string(),
            "CALL rsduck_run_partition_maintenance()".to_string(),
            SqlRoute::Write,
            "CALL".to_string(),
        )
        .await
}

pub fn shutdown_workers() {
    if let Some(engine) = DB_ENGINE.get() {
        engine.shutdown();
    }
}

fn engine() -> &'static DbEngine {
    DB_ENGINE.get().expect("db not initialized")
}

impl DbEngine {
    async fn query(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> Result<SqlResult, String> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        send_sql(&self.read_txs[idx], username, sql, route, command, "read").await
    }

    async fn execute(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> Result<SqlResult, String> {
        send_sql(&self.write_tx, username, sql, route, command, "write").await
    }

    async fn save_snapshot(
        &self,
        username: Option<String>,
        dir: String,
        prefix: String,
    ) -> Result<String, String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.snapshot_tx.try_send(SnapshotCommand::Save {
            username,
            dir,
            prefix,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("snapshot worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("snapshot queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("snapshot worker stopped".into()),
        }
    }

    async fn authenticate(&self, username: String, password: String) -> Result<(), String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.write_tx.try_send(SqlCommand::Authenticate {
            username,
            password,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("write worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("write queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("write worker stopped".into()),
        }
    }

    async fn evaluate_privilege_function(
        &self,
        username: String,
        sql: String,
    ) -> Result<SqlResult, String> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.read_txs[idx].try_send(SqlCommand::PrivilegeFunction {
            username,
            sql,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("read worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("read queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("read worker stopped".into()),
        }
    }

    fn shutdown(&self) {
        let _ = self.write_tx.try_send(SqlCommand::Shutdown);
        for read_tx in &self.read_txs {
            let _ = read_tx.try_send(SqlCommand::Shutdown);
        }
        let _ = self.snapshot_tx.try_send(SnapshotCommand::Shutdown);

        if let Ok(mut workers) = self.workers.lock() {
            while let Some(worker) = workers.pop() {
                if let Err(e) = worker.join() {
                    error!("DuckDB worker thread join failed: {:?}", e);
                }
            }
        }
    }
}

async fn send_sql(
    tx: &SyncSender<SqlCommand>,
    username: String,
    sql: String,
    route: SqlRoute,
    command: String,
    queue_name: &str,
) -> Result<SqlResult, String> {
    let (resp_tx, resp_rx) = oneshot::channel();
    match tx.try_send(SqlCommand::Run {
        username,
        sql,
        route,
        command,
        resp: resp_tx,
    }) {
        Ok(()) => resp_rx
            .await
            .unwrap_or_else(|_| Err(format!("{queue_name} worker stopped"))),
        Err(TrySendError::Full(_)) => Err(format!("{queue_name} queue is full")),
        Err(TrySendError::Disconnected(_)) => Err(format!("{queue_name} worker stopped")),
    }
}

fn spawn_sql_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SqlCommand>,
    max_result_rows: usize,
) -> JoinHandle<()>
where
    N: Into<String>,
{
    let name = name.into();
    let thread_log_name = name.clone();
    thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            info!("DuckDB worker started: {thread_log_name}");
            while let Ok(command) = rx.recv() {
                match command {
                    SqlCommand::Run {
                        username,
                        sql,
                        route,
                        command,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            execute_sql_blocking(
                                &conn,
                                &username,
                                &sql,
                                route,
                                &command,
                                max_result_rows,
                            )
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result);
                    }
                    SqlCommand::Authenticate {
                        username,
                        password,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            crate::catalog::authenticate_user(&conn, &username, &password)
                                .map(|_| ())
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result);
                    }
                    SqlCommand::PrivilegeFunction {
                        username,
                        sql,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            let (column, allowed) = crate::catalog::evaluate_privilege_function(
                                &conn, &username, &sql,
                            )?;
                            Ok(SqlResult::Query {
                                columns: vec![column],
                                rows: vec![vec![if allowed { "t" } else { "f" }.to_string()]],
                            })
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result);
                    }
                    SqlCommand::Shutdown => break,
                }
            }
            info!("DuckDB worker stopped: {thread_log_name}");
        })
        .unwrap_or_else(|e| panic!("spawn DuckDB worker {name} failed: {e}"))
}

fn spawn_snapshot_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SnapshotCommand>,
) -> JoinHandle<()>
where
    N: Into<String>,
{
    let name = name.into();
    let thread_log_name = name.clone();
    thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            info!("DuckDB snapshot worker started: {thread_log_name}");
            while let Ok(command) = rx.recv() {
                match command {
                    SnapshotCommand::Save {
                        username,
                        dir,
                        prefix,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            if let Some(username) = username.as_deref() {
                                crate::catalog::authorize_snapshot(&conn, username)?;
                            }
                            save_snapshot_blocking(&conn, &dir, &prefix)
                        }))
                        .unwrap_or_else(|e| Err(format!("snapshot worker panicked: {e:?}")));
                        match &result {
                            Ok(path) => info!(
                                target: "rsduck_audit",
                                event = "snapshot_save",
                                username = username.as_deref().unwrap_or("system"),
                                path = path.as_str()
                            ),
                            Err(error) => error!(
                                target: "rsduck_audit",
                                event = "snapshot_save_failed",
                                username = username.as_deref().unwrap_or("system"),
                                error = error.as_str()
                            ),
                        }
                        let _ = resp.send(result);
                    }
                    SnapshotCommand::Shutdown => break,
                }
            }
            info!("DuckDB snapshot worker stopped: {thread_log_name}");
        })
        .unwrap_or_else(|e| panic!("spawn DuckDB snapshot worker {name} failed: {e}"))
}

fn restore_or_initialize(
    conn: &Connection,
    snapshot_dir: Option<&str>,
    init_sql_path: &str,
) -> Result<(), String> {
    if let Some(path) = snapshot_dir {
        let t0 = Instant::now();
        info!("Restoring from snapshot dir: {}", path);
        prepare_snapshot_parquet_extension(conn, Path::new(path).parent())?;
        conn.execute_batch(&import_database_sql(path))
            .map_err(|e| format!("import snapshot failed: {e}"))?;
        info!("Snapshot restored in {:.2?}", t0.elapsed());
        validate_snapshot_manifest(conn, Path::new(path))?;
        crate::catalog::validate_after_start(conn)?;
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

fn execute_sql_blocking(
    conn: &Connection,
    username: &str,
    sql: &str,
    route: SqlRoute,
    command: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if let Some(result) = crate::pg_compat::compat_result(sql_trimmed, username) {
        return Ok(result);
    }
    if let Some(rewritten_sql) = crate::pg_compat::rewrite_sql(sql_trimmed) {
        crate::catalog::authorize_catalog_projection(conn, username)?;
        return query_sql_blocking(conn, &rewritten_sql, max_result_rows);
    }
    if crate::catalog::is_reserved_diagnostic_read(sql_trimmed) {
        crate::catalog::authorize_reserved_diagnostic(conn, username, sql_trimmed)?;
        return query_sql_blocking(conn, sql_trimmed, max_result_rows);
    }
    crate::catalog::guard_external_sql_as(username, sql_trimmed)?;
    crate::catalog::reject_unhandled_catalog_projection(sql_trimmed)?;
    crate::catalog::authorize_sql(conn, username, sql_trimmed)?;

    match route {
        SqlRoute::Read => query_sql_blocking(conn, sql_trimmed, max_result_rows),
        SqlRoute::Write => {
            let affected_rows = match crate::catalog::execute_catalog_aware_write_as(
                conn,
                username,
                sql_trimmed,
            )? {
                Some(affected_rows) => affected_rows,
                None => conn.execute(sql_trimmed, []).map_err(|e| e.to_string())?,
            };
            Ok(SqlResult::Execute {
                command: command.to_string(),
                affected_rows,
            })
        }
    }
}

fn query_sql_blocking(
    conn: &Connection,
    sql: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "query did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    let cols: Vec<String> = (0..col_count)
        .map(|idx| {
            stmt_ref
                .column_name(idx)
                .map(|name| name.to_string())
                .unwrap_or_else(|_| format!("column_{idx}"))
        })
        .collect();
    let mut data = Vec::new();

    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if data.len() >= max_result_rows {
            return Err(format!("result row limit exceeded: {max_result_rows}"));
        }
        let mut line = Vec::with_capacity(cols.len());
        for idx in 0..cols.len() {
            line.push(cell_to_string(row, idx));
        }
        data.push(line);
    }

    Ok(SqlResult::Query {
        columns: cols,
        rows: data,
    })
}

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

fn cell_to_string(row: &duckdb::Row<'_>, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(value) => value_ref_to_string(value),
        Err(_) => String::new(),
    }
}

fn value_ref_to_string(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => String::new(),
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

#[cfg(test)]
mod tests {
    use super::{
        execute_sql_blocking, export_database_sql, find_latest_snapshot_dir, import_database_sql,
        parse_snapshot_dir_timestamp, restore_or_initialize, save_snapshot_blocking, SqlResult,
        SNAPSHOT_MANIFEST_FILE,
    };
    use crate::sql_route::route_sql;
    use duckdb::Connection;
    use std::path::PathBuf;

    #[test]
    fn parse_snapshot_dir_timestamp_only_accepts_final_snapshot_dirs() {
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500", "rsduck").is_some());
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500.tmp", "rsduck").is_none());
        assert!(parse_snapshot_dir_timestamp("rsduck_20260702_101500.parquet", "rsduck").is_none());
        assert!(parse_snapshot_dir_timestamp("rsduck_latest", "rsduck").is_none());
    }

    #[test]
    fn find_latest_snapshot_dir_uses_newest_final_snapshot_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_test_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let dirs = [
            "rsduck_20260702_101500",
            "rsduck_20260702_101700.tmp",
            "rsduck_20260702_101600",
            "other_20260702_101900",
        ];
        for dir_name in dirs {
            std::fs::create_dir_all(dir.join(dir_name)).unwrap();
        }
        std::fs::write(dir.join("rsduck_20260702_101800.parquet"), b"").unwrap();
        std::fs::write(dir.join("rsduck_20260702_101900.parquet.tmp"), b"").unwrap();

        let latest = find_latest_snapshot_dir(dir.to_str().unwrap(), "rsduck").unwrap();
        assert_eq!(
            PathBuf::from(latest).file_name().unwrap().to_string_lossy(),
            "rsduck_20260702_101600"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catalog_projection_rewrite_executes_through_db_auth_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE quotes(code VARCHAR)")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER alice PASSWORD='pw'")
            .unwrap();

        let sql = "SELECT relname FROM pg_catalog.pg_class WHERE relname = 'quotes'";
        let decision = route_sql(sql).unwrap();
        let result =
            execute_sql_blocking(&conn, "alice", sql, decision.route, &decision.command, 100)
                .unwrap();

        let SqlResult::Query { columns, rows } = result else {
            panic!("expected catalog projection query result");
        };
        let relname_idx = columns
            .iter()
            .position(|column| column == "relname")
            .expect("relname column");
        assert!(rows.iter().any(|row| row[relname_idx] == "quotes"));
    }

    #[test]
    fn internal_catalog_query_requires_catalog_diagnostic_privilege() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE USER operator_user PASSWORD='pw'",
        )
        .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE USER plain_user PASSWORD='pw'")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "GRANT ROLE operator TO operator_user")
            .unwrap();

        let sql = "SELECT * FROM rsduck_catalog.pg_class";
        let decision = route_sql(sql).unwrap();
        execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100).unwrap();
        execute_sql_blocking(
            &conn,
            "operator_user",
            sql,
            decision.route,
            &decision.command,
            100,
        )
        .unwrap();
        let err = execute_sql_blocking(
            &conn,
            "plain_user",
            sql,
            decision.route,
            &decision.command,
            100,
        )
        .unwrap_err();
        assert!(err.contains("manage_catalog"));
    }

    #[test]
    fn reserved_pg_catalog_write_is_rejected_through_db_path() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        let sql = "INSERT INTO pg_catalog.pg_class VALUES (1)";
        let decision = route_sql(sql).unwrap();
        let err = execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap_err();
        assert_eq!(
            err,
            "reserved schema is managed by rsduck catalog: pg_catalog"
        );
    }

    #[test]
    fn unsupported_catalog_relation_reports_relation_name() {
        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();

        let sql = "SELECT * FROM pg_catalog.pg_am";
        let decision = route_sql(sql).unwrap();
        let err = execute_sql_blocking(&conn, "admin", sql, decision.route, &decision.command, 100)
            .unwrap_err();
        assert_eq!(err, "unsupported pg_catalog relation: pg_am");
    }

    #[test]
    fn snapshot_directory_round_trip_restores_multiple_tables() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_round_trip_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE table_a(id INTEGER, name VARCHAR)",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO table_a VALUES (1, 'alpha'), (2, 'beta');")
            .unwrap();
        crate::catalog::execute_catalog_aware_write(
            &conn,
            "CREATE TABLE table_b(id INTEGER, amount DOUBLE)",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO table_b VALUES (10, 1.5);")
            .unwrap();

        let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
        let (catalog_epoch, catalog_checksum): (i64, String) = conn
            .query_row(
                "SELECT catalog_epoch, catalog_checksum \
                 FROM rsduck_catalog.rs_catalog_version \
                 WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let manifest_path = PathBuf::from(&snapshot).join("rsduck_snapshot_manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["manifest_version"], 1);
        assert_eq!(manifest["catalog_epoch"], catalog_epoch);
        assert_eq!(manifest["catalog_checksum"], catalog_checksum);
        assert_eq!(
            manifest["snapshot_name"],
            PathBuf::from(&snapshot)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );

        let restored = Connection::open_in_memory().unwrap();
        restore_or_initialize(&restored, Some(&snapshot), "").unwrap();

        let table_a_count: i64 = restored
            .query_row("SELECT COUNT(*) FROM table_a", [], |row| row.get(0))
            .unwrap();
        let table_b_count: i64 = restored
            .query_row("SELECT COUNT(*) FROM table_b", [], |row| row.get(0))
            .unwrap();
        assert_eq!(table_a_count, 2);
        assert_eq!(table_b_count, 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_restore_rejects_manifest_checksum_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_manifest_mismatch_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        let conn = Connection::open_in_memory().unwrap();
        crate::catalog::bootstrap_fresh(&conn).unwrap();
        crate::catalog::execute_catalog_aware_write(&conn, "CREATE TABLE table_a(id INTEGER)")
            .unwrap();

        let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
        let manifest_path = PathBuf::from(&snapshot).join(SNAPSHOT_MANIFEST_FILE);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["catalog_checksum"] = serde_json::Value::String("tampered".to_string());
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let restored = Connection::open_in_memory().unwrap();
        let err = restore_or_initialize(&restored, Some(&snapshot), "").unwrap_err();
        assert!(err.contains("snapshot manifest catalog_checksum mismatch"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn export_and_import_sql_escape_paths() {
        assert_eq!(
            export_database_sql("snapshot/rsduck's.tmp"),
            "EXPORT DATABASE 'snapshot/rsduck''s.tmp' (FORMAT parquet, COMPRESSION zstd)"
        );
        assert_eq!(
            import_database_sql("snapshot/rsduck's"),
            "IMPORT DATABASE 'snapshot/rsduck''s'"
        );
    }
}

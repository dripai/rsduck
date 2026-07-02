use crate::config::DbConfig;
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
        sql: String,
        resp: oneshot::Sender<Result<SqlResult, String>>,
    },
    Shutdown,
}

enum SnapshotCommand {
    Save {
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

pub async fn execute_sql(sql: String) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim().to_string();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if is_query_sql(&sql_trimmed) {
        engine().query(sql_trimmed).await
    } else {
        engine().execute(sql_trimmed).await
    }
}

pub async fn save_snapshot(snapshot_dir: &str, snapshot_prefix: &str) -> Result<String, String> {
    engine()
        .save_snapshot(snapshot_dir.to_string(), snapshot_prefix.to_string())
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
    async fn query(&self, sql: String) -> Result<SqlResult, String> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        send_sql(&self.read_txs[idx], sql, "read").await
    }

    async fn execute(&self, sql: String) -> Result<SqlResult, String> {
        send_sql(&self.write_tx, sql, "write").await
    }

    async fn save_snapshot(&self, dir: String, prefix: String) -> Result<String, String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.snapshot_tx.try_send(SnapshotCommand::Save {
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
    sql: String,
    queue_name: &str,
) -> Result<SqlResult, String> {
    let (resp_tx, resp_rx) = oneshot::channel();
    match tx.try_send(SqlCommand::Run { sql, resp: resp_tx }) {
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
                    SqlCommand::Run { sql, resp } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            execute_sql_blocking(&conn, &sql, max_result_rows)
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
                    SnapshotCommand::Save { dir, prefix, resp } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            save_snapshot_blocking(&conn, &dir, &prefix)
                        }))
                        .unwrap_or_else(|e| Err(format!("snapshot worker panicked: {e:?}")));
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
        conn.execute_batch(&import_database_sql(path))
            .map_err(|e| format!("import snapshot failed: {e}"))?;
        info!("Snapshot restored in {:.2?}", t0.elapsed());
        return Ok(());
    }

    let init_sql_path = init_sql_path.trim();
    if init_sql_path.is_empty() {
        info!("No snapshot dir found and init_sql is empty, starting empty in-memory DuckDB");
        return Ok(());
    }

    let path = Path::new(init_sql_path);
    if !path.is_file() {
        return Err(format!("init_sql file not found: {init_sql_path}"));
    }

    let t0 = Instant::now();
    info!("Initializing DuckDB from init_sql: {}", init_sql_path);
    let sql = fs::read_to_string(path).map_err(|e| format!("read init_sql failed: {e}"))?;
    conn.execute_batch(&sql)
        .map_err(|e| format!("execute init_sql failed: {e}"))?;
    info!("init_sql executed in {:.2?}", t0.elapsed());
    Ok(())
}

fn execute_sql_blocking(
    conn: &Connection,
    sql: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    let command = detect_command(sql_trimmed);
    if is_query_command(&command) {
        query_sql_blocking(conn, sql_trimmed, max_result_rows)
    } else {
        let affected_rows = conn.execute(sql_trimmed, []).map_err(|e| e.to_string())?;
        Ok(SqlResult::Execute {
            command,
            affected_rows,
        })
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

    let tmp_path_text = tmp_path.display().to_string();
    conn.execute_batch(&export_database_sql(&tmp_path_text))
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_path);
            format!("export snapshot failed: {e}")
        })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp_path);
        format!("rename snapshot dir failed: {e}")
    })?;
    Ok(final_path.display().to_string())
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

fn is_query_sql(sql: &str) -> bool {
    is_query_command(&detect_command(sql))
}

fn detect_command(sql: &str) -> String {
    sql.split_whitespace()
        .next()
        .unwrap_or("OK")
        .to_ascii_uppercase()
}

fn is_query_command(command: &str) -> bool {
    matches!(
        command,
        "SELECT" | "SHOW" | "WITH" | "DESCRIBE" | "EXPLAIN" | "PRAGMA"
    )
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
        export_database_sql, find_latest_snapshot_dir, import_database_sql,
        parse_snapshot_dir_timestamp, restore_or_initialize, save_snapshot_blocking,
    };
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
    fn snapshot_directory_round_trip_restores_multiple_tables() {
        let dir = std::env::temp_dir().join(format!(
            "rsduck_snapshot_round_trip_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE table_a(id INTEGER, name VARCHAR);
            INSERT INTO table_a VALUES (1, 'alpha'), (2, 'beta');
            CREATE TABLE table_b(id INTEGER, amount DOUBLE);
            INSERT INTO table_b VALUES (10, 1.5);
            ",
        )
        .unwrap();

        let snapshot = save_snapshot_blocking(&conn, dir.to_str().unwrap(), "rsduck").unwrap();
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

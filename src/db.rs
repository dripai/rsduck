use duckdb::{types::ValueRef, Connection};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tokio::task;
use tracing::{info, warn};

static DB_INSTANCE: OnceLock<Mutex<Connection>> = OnceLock::new();

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

pub fn init_db(snapshot_file: Option<&str>) {
    let conn = Connection::open("").expect("open in-memory duckdb failed");

    match snapshot_file {
        Some(path) if Path::new(path).exists() => {
            let t0 = Instant::now();
            info!("Restoring from snapshot: {}", path);
            conn.execute(
                &format!(
                    "CREATE TABLE kline_day AS SELECT * FROM read_parquet('{}')",
                    escape_sql_string(path)
                ),
                [],
            )
            .expect("restore from snapshot failed");
            let row_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM kline_day", [], |r| r.get(0))
                .unwrap_or(0);
            info!(
                "Snapshot restored: {} rows in {:.2?}",
                row_count,
                t0.elapsed()
            );
        }
        _ => {
            if let Some(path) = snapshot_file {
                warn!("Snapshot file {} not found, starting fresh", path);
            }
            create_schema(&conn);
        }
    }

    DB_INSTANCE
        .set(Mutex::new(conn))
        .unwrap_or_else(|_| panic!("db initialized twice"));
}

fn create_schema(conn: &Connection) {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS kline_day (
            code      VARCHAR NOT NULL,
            bar_time  TIMESTAMP NOT NULL,
            open      DOUBLE,
            high      DOUBLE,
            low       DOUBLE,
            close     DOUBLE,
            volume    BIGINT,
            PRIMARY KEY (code, bar_time)
        )",
        [],
    )
    .expect("create table kline_day failed");
}

fn db_mutex() -> &'static Mutex<Connection> {
    DB_INSTANCE.get().expect("db not initialized")
}

pub async fn execute_sql(sql: String) -> Result<SqlResult, String> {
    match task::spawn_blocking(move || execute_sql_blocking(&sql)).await {
        Ok(result) => result,
        Err(e) => Err(format!("duckdb worker panicked: {e}")),
    }
}

fn execute_sql_blocking(sql: &str) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    let command = detect_command(sql_trimmed);
    let conn = db_mutex().lock().map_err(|e| e.to_string())?;

    if is_query_command(&command) {
        let mut stmt = conn.prepare(sql_trimmed).map_err(|e| e.to_string())?;
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
    } else {
        let affected_rows = conn.execute(sql_trimmed, []).map_err(|e| e.to_string())?;
        Ok(SqlResult::Execute {
            command,
            affected_rows,
        })
    }
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

pub fn find_latest_snapshot(snapshot_dir: &str, table_prefix: &str) -> Option<String> {
    let base = Path::new(snapshot_dir);
    if !base.exists() {
        return None;
    }

    let prefix = format!("{table_prefix}_");
    let mut files: Vec<String> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(&prefix) && name.ends_with(".parquet"))
        .collect();

    files.sort_by(|a, b| b.cmp(a));
    files
        .first()
        .map(|name| PathBuf::from(snapshot_dir).join(name).display().to_string())
}

pub fn snapshot_sql(snapshot_path: &str) -> String {
    format!(
        "COPY kline_day TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
        escape_sql_string(snapshot_path)
    )
}

pub async fn save_snapshot(snapshot_dir: &str) -> Result<String, String> {
    std::fs::create_dir_all(snapshot_dir)
        .map_err(|e| format!("create snapshot dir failed: {e}"))?;

    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = Path::new(snapshot_dir)
        .join(format!("kline_day_{ts}.parquet"))
        .display()
        .to_string();
    let sql = snapshot_sql(&path);
    execute_sql(sql).await.map(|_| path)
}

fn escape_sql_string(input: &str) -> String {
    input.replace('\'', "''")
}

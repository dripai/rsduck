use crate::auth::{AuthRequest, AuthenticatedPrincipal};
use crate::sql_route::SqlRoute;
use duckdb::Connection;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

use super::error::DbResult;

pub(super) const SNAPSHOT_MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SqlType {
    Bool,
    Bytea,
    Int8,
    Int2,
    Int4,
    Text,
    Float4,
    Float8,
    Numeric,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
}

impl SqlType {
    pub fn sql_type_name(self) -> &'static str {
        match self {
            SqlType::Bool => "boolean",
            SqlType::Bytea => "binary",
            SqlType::Int8 => "bigint",
            SqlType::Int2 => "smallint",
            SqlType::Int4 => "integer",
            SqlType::Text => "text",
            SqlType::Float4 => "real",
            SqlType::Float8 => "double",
            SqlType::Numeric => "decimal",
            SqlType::Date => "date",
            SqlType::Time => "time",
            SqlType::Timestamp => "timestamp",
            SqlType::TimestampTz => "timestamp with time zone",
            SqlType::Uuid => "uuid",
            SqlType::Json => "json",
        }
    }

    pub fn mysql_type_name(self) -> &'static str {
        match self {
            SqlType::Bool => "tinyint",
            SqlType::Bytea => "blob",
            SqlType::Int8 => "bigint",
            SqlType::Int2 => "smallint",
            SqlType::Int4 => "int",
            SqlType::Text => "varchar",
            SqlType::Float4 => "float",
            SqlType::Float8 => "double",
            SqlType::Numeric => "decimal",
            SqlType::Date => "date",
            SqlType::Time => "time",
            SqlType::Timestamp | SqlType::TimestampTz => "datetime",
            SqlType::Uuid => "char",
            SqlType::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SqlColumn {
    pub name: String,
    pub data_type: SqlType,
}

impl SqlColumn {
    pub(super) fn new(name: impl Into<String>, data_type: SqlType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Float32(f32),
    Float64(f64),
    Decimal(rust_decimal::Decimal),
    NumericText(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(chrono::NaiveDate),
    Time(chrono::NaiveTime),
    Timestamp(chrono::NaiveDateTime),
    TimestampTz(chrono::DateTime<chrono::Utc>),
    Uuid(uuid::Uuid),
    Json(serde_json::Value),
    Interval { months: i32, days: i32, nanos: i64 },
}

impl SqlValue {
    pub fn text_value(&self) -> Option<String> {
        match self {
            SqlValue::Null => None,
            SqlValue::Bool(value) => Some(if *value { "t" } else { "f" }.to_string()),
            SqlValue::Int16(value) => Some(value.to_string()),
            SqlValue::Int32(value) => Some(value.to_string()),
            SqlValue::Int64(value) => Some(value.to_string()),
            SqlValue::Float32(value) => Some(value.to_string()),
            SqlValue::Float64(value) => Some(value.to_string()),
            SqlValue::Decimal(value) => Some(value.to_string()),
            SqlValue::NumericText(value) => Some(value.clone()),
            SqlValue::Text(value) => Some(value.clone()),
            SqlValue::Bytes(value) => Some(format!("\\x{}", hex_encode(value))),
            SqlValue::Date(value) => Some(value.format("%Y-%m-%d").to_string()),
            SqlValue::Time(value) => Some(value.format("%H:%M:%S%.6f").to_string()),
            SqlValue::Timestamp(value) => Some(value.format("%Y-%m-%d %H:%M:%S%.6f").to_string()),
            SqlValue::TimestampTz(value) => {
                Some(value.format("%Y-%m-%d %H:%M:%S%.6f%:z").to_string())
            }
            SqlValue::Uuid(value) => Some(value.to_string()),
            SqlValue::Json(value) => Some(value.to_string()),
            SqlValue::Interval {
                months,
                days,
                nanos,
            } => Some(format!("{months} months {days} days {nanos} ns")),
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

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

#[derive(Debug, Clone)]
pub enum SqlTypedResult {
    Query {
        columns: Vec<SqlColumn>,
        rows: Vec<Vec<SqlValue>>,
    },
    Execute {
        command: String,
        affected_rows: usize,
    },
}

#[derive(Clone)]
pub struct DbHandle {
    pub(super) engine: Arc<DbEngine>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ParquetImportSource {
    pub table: String,
    pub path: String,
}

impl From<SqlTypedResult> for SqlResult {
    fn from(result: SqlTypedResult) -> Self {
        match result {
            SqlTypedResult::Query { columns, rows } => SqlResult::Query {
                columns: columns.into_iter().map(|column| column.name).collect(),
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| cell.text_value().unwrap_or_default())
                            .collect()
                    })
                    .collect(),
            },
            SqlTypedResult::Execute {
                command,
                affected_rows,
            } => SqlResult::Execute {
                command,
                affected_rows,
            },
        }
    }
}

pub(super) enum SqlCommand {
    RunTyped {
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
        resp: oneshot::Sender<DbResult<SqlTypedResult>>,
    },
    Authenticate {
        request: AuthRequest,
        resp: oneshot::Sender<DbResult<AuthenticatedPrincipal>>,
    },
    Describe {
        username: String,
        sql: String,
        route: SqlRoute,
        resp: oneshot::Sender<DbResult<Vec<SqlColumn>>>,
    },
    ImportParquet {
        username: String,
        schema: String,
        sources: Vec<ParquetImportSource>,
        resp: oneshot::Sender<DbResult<usize>>,
    },
    Shutdown,
}

pub(super) enum SnapshotCommand {
    Save {
        username: Option<String>,
        dir: String,
        prefix: String,
        resp: oneshot::Sender<DbResult<String>>,
    },
    Shutdown,
}

pub(super) struct DbEngine {
    pub(super) read_txs: Vec<SyncSender<SqlCommand>>,
    pub(super) write_tx: SyncSender<SqlCommand>,
    pub(super) snapshot_tx: SyncSender<SnapshotCommand>,
    pub(super) next_read: AtomicUsize,
    pub(super) _base_conn: Mutex<Connection>,
    pub(super) workers: Mutex<Vec<JoinHandle<()>>>,
}

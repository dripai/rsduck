use crate::auth::{AuthRequest, AuthenticatedPrincipal};
use crate::sql_route::SqlRoute;
use duckdb::Connection;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

use super::error::DbResult;

pub(super) const SNAPSHOT_MANIFEST_FILE: &str = "rsduck_snapshot_manifest.json";

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
    pub fn pg_type_oid(self) -> u32 {
        match self {
            SqlType::Bool => 16,
            SqlType::Bytea => 17,
            SqlType::Int8 => 20,
            SqlType::Int2 => 21,
            SqlType::Int4 => 23,
            SqlType::Text => 25,
            SqlType::Float4 => 700,
            SqlType::Float8 => 701,
            SqlType::Numeric => 1700,
            SqlType::Date => 1082,
            SqlType::Time => 1083,
            SqlType::Timestamp => 1114,
            SqlType::TimestampTz => 1184,
            SqlType::Uuid => 2950,
            SqlType::Json => 25,
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

    pub(super) fn text(name: impl Into<String>) -> Self {
        Self::new(name, SqlType::Text)
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

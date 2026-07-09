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

const PG_TYPE_BOOL: u32 = 16;
const PG_TYPE_BYTEA: u32 = 17;
const PG_TYPE_INT8: u32 = 20;
const PG_TYPE_INT2: u32 = 21;
const PG_TYPE_INT4: u32 = 23;
const PG_TYPE_TEXT: u32 = 25;
const PG_TYPE_FLOAT4: u32 = 700;
const PG_TYPE_FLOAT8: u32 = 701;
const PG_TYPE_NUMERIC: u32 = 1700;
const PG_TYPE_DATE: u32 = 1082;
const PG_TYPE_TIME: u32 = 1083;
const PG_TYPE_TIMESTAMP: u32 = 1114;
const PG_TYPE_TIMESTAMPTZ: u32 = 1184;
const PG_TYPE_UUID: u32 = 2950;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SqlColumn {
    pub name: String,
    pub pg_type_oid: u32,
}

impl SqlColumn {
    fn new(name: impl Into<String>, pg_type_oid: u32) -> Self {
        Self {
            name: name.into(),
            pg_type_oid,
        }
    }

    fn text(name: impl Into<String>) -> Self {
        Self::new(name, PG_TYPE_TEXT)
    }
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
        rows: Vec<Vec<Option<String>>>,
    },
    Execute {
        command: String,
        affected_rows: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Null,
    Text(String),
    Bool(bool),
    Integer(i64),
    Float(f64),
    Bytes(Vec<u8>),
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
                            .map(|cell| cell.unwrap_or_default())
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

enum SqlCommand {
    RunTyped {
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
        resp: oneshot::Sender<Result<SqlTypedResult, String>>,
    },
    Authenticate {
        username: String,
        password: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    Describe {
        username: String,
        sql: String,
        route: SqlRoute,
        resp: oneshot::Sender<Result<Vec<SqlColumn>, String>>,
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

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

pub(super) enum SqlCommand {
    RunTyped {
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
        resp: oneshot::Sender<DbResult<SqlTypedResult>>,
    },
    Authenticate {
        username: String,
        password: String,
        resp: oneshot::Sender<DbResult<()>>,
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

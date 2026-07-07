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


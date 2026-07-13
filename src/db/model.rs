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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct VectorIndexCreate {
    pub vector_space: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub index_name: String,
    pub embedding_model: String,
    pub model_version: String,
    pub metric: String,
    #[serde(default = "default_vector_m")]
    pub m: i32,
    #[serde(default = "default_vector_m0")]
    pub m0: i32,
    #[serde(default = "default_vector_ef_construction")]
    pub ef_construction: i32,
    #[serde(default = "default_vector_ef_search")]
    pub default_ef_search: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorIndexInfo {
    pub index_oid: i64,
    pub vector_space: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub index_name: String,
    pub embedding_model: String,
    pub model_version: String,
    pub dimension: usize,
    pub metric: String,
    pub m: i32,
    pub m0: i32,
    pub ef_construction: i32,
    pub default_ef_search: i32,
    pub definition_version: i64,
    pub generation: i64,
    pub extension_version: String,
    pub build_status: String,
    pub vector_count: i64,
    pub built_at: String,
    pub updated_at: String,
    pub error_message: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VectorSearchRequest {
    pub vector_space: String,
    pub tenant_id: i64,
    pub agent_id: i64,
    pub embedding: Vec<f32>,
    pub top_k: usize,
    pub mode: String,
    pub ef_search: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct VectorMatch {
    pub memory_id: i64,
    pub distance: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorSearchResult {
    pub vector_space: String,
    pub mode: String,
    pub index_status: String,
    pub matches: Vec<VectorMatch>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VectorUpsertItem {
    pub tenant_id: i64,
    pub agent_id: i64,
    pub memory_id: i64,
    pub source_version: i64,
    pub content_hash: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VectorUpsertRequest {
    pub vector_space: String,
    pub items: Vec<VectorUpsertItem>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VectorDeleteItem {
    pub tenant_id: i64,
    pub agent_id: i64,
    pub memory_id: i64,
    pub source_version: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct VectorDeleteRequest {
    pub vector_space: String,
    pub items: Vec<VectorDeleteItem>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorMutationResult {
    pub vector_space: String,
    pub applied: usize,
    pub idempotent: usize,
    pub vector_count: i64,
}

impl From<crate::catalog::VectorIndexStatus> for VectorIndexInfo {
    fn from(status: crate::catalog::VectorIndexStatus) -> Self {
        Self {
            index_oid: status.index_oid,
            vector_space: status.vector_space,
            schema: status.schema,
            table: status.table,
            column: status.column,
            index_name: status.index_name,
            embedding_model: status.embedding_model,
            model_version: status.model_version,
            dimension: status.dimension,
            metric: status.metric,
            m: status.m,
            m0: status.m0,
            ef_construction: status.ef_construction,
            default_ef_search: status.default_ef_search,
            definition_version: status.definition_version,
            generation: status.generation,
            extension_version: status.extension_version,
            build_status: status.build_status,
            vector_count: status.vector_count,
            built_at: status.built_at,
            updated_at: status.updated_at,
            error_message: status.error_message,
        }
    }
}

fn default_vector_m() -> i32 {
    16
}

fn default_vector_m0() -> i32 {
    32
}

fn default_vector_ef_construction() -> i32 {
    128
}

fn default_vector_ef_search() -> i32 {
    64
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
    CreateVectorIndex {
        username: String,
        request: VectorIndexCreate,
        resp: oneshot::Sender<DbResult<VectorIndexInfo>>,
    },
    VectorIndexStatus {
        username: String,
        vector_space: String,
        resp: oneshot::Sender<DbResult<VectorIndexInfo>>,
    },
    VectorSearch {
        username: String,
        request: VectorSearchRequest,
        resp: oneshot::Sender<DbResult<VectorSearchResult>>,
    },
    VectorUpsert {
        username: String,
        request: VectorUpsertRequest,
        resp: oneshot::Sender<DbResult<VectorMutationResult>>,
    },
    VectorDelete {
        username: String,
        request: VectorDeleteRequest,
        resp: oneshot::Sender<DbResult<VectorMutationResult>>,
    },
    RebuildVectorIndex {
        username: String,
        vector_space: String,
        resp: oneshot::Sender<DbResult<VectorIndexInfo>>,
    },
    CompactVectorIndex {
        username: String,
        vector_space: String,
        resp: oneshot::Sender<DbResult<VectorIndexInfo>>,
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

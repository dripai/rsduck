use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Json, Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time;

use crate::auth::{AuthProtocol, AuthRequest};
use crate::config::{VectorApiLimitsConfig, VectorApiTokenConfig};
use crate::db::{
    DbHandle, ParquetImportSource, SqlTypedResult, SqlValue, VectorDeleteRequest,
    VectorIndexCreate, VectorIndexInfo, VectorMutationResult, VectorSearchRequest,
    VectorSearchResult, VectorUpsertRequest,
};

use super::web_assets::{CODEMIRROR_JS, INDEX_HTML};

#[derive(Debug, Deserialize)]
pub struct SqlReq {
    pub sql: String,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Debug, Serialize)]
pub struct SqlResp {
    pub columns: Vec<SqlRespColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    pub success: bool,
    pub msg: String,
}

#[derive(Debug, Serialize)]
pub struct SqlRespColumn {
    pub name: String,
    pub sql_type: String,
    pub mysql_type: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResp {
    pub success: bool,
    pub msg: String,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct SessionResp {
    pub authenticated: bool,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResp {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct ParquetImportReq {
    pub source: String,
    pub schema: String,
    pub table: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ParquetImportResp {
    pub success: bool,
    pub msg: String,
    pub tables: Vec<String>,
    pub rows: usize,
}

#[derive(Debug, Serialize)]
pub struct ParquetImportInfoResp {
    pub success: bool,
    pub root: String,
    pub msg: String,
}

#[derive(Debug, Serialize)]
pub struct VectorIndexResp {
    pub success: bool,
    pub error_code: Option<String>,
    pub trace_id: String,
    pub msg: String,
    pub index: Option<VectorIndexInfo>,
}

#[derive(Debug, Serialize)]
pub struct VectorSearchResp {
    pub success: bool,
    pub error_code: Option<String>,
    pub trace_id: String,
    pub msg: String,
    pub result: Option<VectorSearchResult>,
}

#[derive(Debug, Serialize)]
pub struct VectorMutationResp {
    pub success: bool,
    pub error_code: Option<String>,
    pub trace_id: String,
    pub msg: String,
    pub result: Option<VectorMutationResult>,
}

#[derive(Clone)]
pub struct WebState {
    pub db: DbHandle,
    pub snapshot_dir: Arc<String>,
    pub snapshot_prefix: Arc<String>,
    pub parquet_import_root: Arc<PathBuf>,
    pub sessions: Arc<Mutex<HashMap<String, String>>>,
    vector_api_tokens: Arc<HashMap<String, VectorServiceIdentity>>,
    vector_api_limits: Arc<VectorApiLimitsConfig>,
    vector_api_semaphore: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
struct VectorServiceIdentity {
    username: String,
    tenant_ids: Vec<i64>,
    agent_ids: Vec<i64>,
    vector_spaces: Vec<String>,
    permissions: Vec<String>,
}

async fn sql_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(req): Json<SqlReq>,
) -> Json<SqlResp> {
    let Some(username) = session_username(&state, &headers) else {
        return Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: false,
            msg: "authentication required".into(),
        });
    };

    let sql = req.sql.trim().to_string();
    if sql.is_empty() {
        return Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: false,
            msg: "empty sql".into(),
        });
    }

    let sql = paged_sql(&sql, req.page, req.page_size);

    match state.db.execute_typed_sql_as(username, sql).await {
        Ok(SqlTypedResult::Query { columns, rows }) => {
            let columns = columns
                .into_iter()
                .map(|column| SqlRespColumn {
                    name: column.name,
                    sql_type: column.data_type.sql_type_name().to_string(),
                    mysql_type: column.data_type.mysql_type_name().to_string(),
                })
                .collect();
            Json(SqlResp {
                columns,
                rows: sql_values_to_resp_rows(rows),
                success: true,
                msg: "ok".into(),
            })
        }
        Ok(SqlTypedResult::Execute {
            command,
            affected_rows,
        }) => Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: true,
            msg: format!("{command} {affected_rows} row(s)"),
        }),
        Err(e) => Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: false,
            msg: e.to_string(),
        }),
    }
}

fn paged_sql(sql: &str, page: usize, page_size: usize) -> String {
    let sql = sql.trim().trim_end_matches(';').trim();
    if !crate::sql_route::is_pageable_sql(sql).unwrap_or(false) {
        return sql.to_string();
    }
    if crate::sql_route::has_top_level_limit_or_offset(sql).unwrap_or(false) {
        return sql.to_string();
    }

    let page_size = page_size.clamp(1, 100_000);
    let offset = page.saturating_mul(page_size);
    format!("SELECT * FROM ({sql}) __rsduck_page LIMIT {page_size} OFFSET {offset}")
}

fn sql_values_to_resp_rows(rows: Vec<Vec<SqlValue>>) -> Vec<Vec<Option<String>>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(|value| value.text_value()).collect())
        .collect()
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health_handler() -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn login_handler(State(state): State<WebState>, Json(req): Json<LoginReq>) -> Response {
    let username = req.username.trim().to_string();
    if username.is_empty() {
        return Json(LoginResp {
            success: false,
            msg: "username is required".into(),
            username: String::new(),
        })
        .into_response();
    }

    match state
        .db
        .authenticate(AuthRequest::cleartext(
            AuthProtocol::WebApi,
            username.clone(),
            req.password,
        ))
        .await
    {
        Ok(_) => {
            let token = new_session_token();
            if let Ok(mut sessions) = state.sessions.lock() {
                sessions.insert(token.clone(), username.clone());
            }
            (
                StatusCode::OK,
                [(
                    header::SET_COOKIE,
                    format!("rsduck_session={token}; Path=/; HttpOnly; SameSite=Lax"),
                )],
                Json(LoginResp {
                    success: true,
                    msg: "ok".into(),
                    username,
                }),
            )
                .into_response()
        }
        Err(e) => Json(LoginResp {
            success: false,
            msg: e.to_string(),
            username: String::new(),
        })
        .into_response(),
    }
}

async fn logout_handler(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if let Some(token) = parse_session_token(&headers) {
        if let Ok(mut sessions) = state.sessions.lock() {
            sessions.remove(&token);
        }
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            "rsduck_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string(),
        )],
        Json(LoginResp {
            success: true,
            msg: "ok".into(),
            username: String::new(),
        }),
    )
        .into_response()
}

async fn session_handler(State(state): State<WebState>, headers: HeaderMap) -> Json<SessionResp> {
    if let Some(username) = session_username(&state, &headers) {
        return Json(SessionResp {
            authenticated: true,
            username,
        });
    }
    Json(SessionResp {
        authenticated: false,
        username: String::new(),
    })
}

async fn codemirror_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        CODEMIRROR_JS,
    )
}

async fn snapshot_handler(State(state): State<WebState>, headers: HeaderMap) -> Json<SqlResp> {
    let Some(username) = session_username(&state, &headers) else {
        return Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: false,
            msg: "authentication required".into(),
        });
    };
    let t0 = Instant::now();

    match state
        .db
        .save_snapshot_as(
            username,
            state.snapshot_dir.as_str(),
            state.snapshot_prefix.as_str(),
        )
        .await
    {
        Ok(path) => Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: true,
            msg: format!("snapshot saved to {path} ({:.2?})", t0.elapsed()),
        }),
        Err(e) => Json(SqlResp {
            columns: vec![],
            rows: vec![],
            success: false,
            msg: format!("snapshot failed: {e}"),
        }),
    }
}

async fn parquet_import_info_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Json<ParquetImportInfoResp> {
    if session_username(&state, &headers).is_none() {
        return Json(ParquetImportInfoResp {
            success: false,
            root: String::new(),
            msg: "authentication required".into(),
        });
    }
    Json(ParquetImportInfoResp {
        success: true,
        root: state.parquet_import_root.display().to_string(),
        msg: "ok".into(),
    })
}

async fn parquet_import_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(req): Json<ParquetImportReq>,
) -> Json<ParquetImportResp> {
    let Some(username) = session_username(&state, &headers) else {
        return Json(ParquetImportResp {
            success: false,
            msg: "authentication required".into(),
            tables: vec![],
            rows: 0,
        });
    };
    let t0 = Instant::now();
    let sources = match resolve_parquet_import_sources(&state.parquet_import_root, &req) {
        Ok(sources) => sources,
        Err(msg) => {
            return Json(ParquetImportResp {
                success: false,
                msg,
                tables: vec![],
                rows: 0,
            });
        }
    };
    let tables = sources
        .iter()
        .map(|source| format!("{}.{}", req.schema, source.table))
        .collect::<Vec<_>>();

    match state
        .db
        .import_parquet_tables_as(username, req.schema, sources)
        .await
    {
        Ok(rows) => Json(ParquetImportResp {
            success: true,
            msg: format!(
                "imported {} table(s), {rows} row(s) ({:.2?})",
                tables.len(),
                t0.elapsed()
            ),
            tables,
            rows,
        }),
        Err(e) => Json(ParquetImportResp {
            success: false,
            msg: format!("Parquet import failed: {e}"),
            tables: vec![],
            rows: 0,
        }),
    }
}

async fn vector_index_create_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    payload: Result<Json<VectorIndexCreate>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => return vector_index_error(vector_json_rejection(&rejection)),
    };
    let Some(username) = session_username(&state, &headers) else {
        return vector_index_error("AUTHENTICATION_FAILED: authentication required".into());
    };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_index_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.maintenance_timeout_ms,
        permit,
        async move { db.create_vector_index_as(username, request).await },
    )
    .await
    {
        Ok(index) => Json(VectorIndexResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            index: Some(index),
        })
        .into_response(),
        Err(error) => vector_index_error(error.to_string()),
    }
}

async fn vector_index_status_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(vector_space): AxumPath<String>,
) -> Response {
    let Some(username) = session_username(&state, &headers) else {
        return vector_index_error("AUTHENTICATION_FAILED: authentication required".into());
    };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_index_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.search_timeout_ms,
        permit,
        async move { db.vector_index_status_as(username, vector_space).await },
    )
    .await
    {
        Ok(index) => Json(VectorIndexResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            index: Some(index),
        })
        .into_response(),
        Err(error) => vector_index_error(error.to_string()),
    }
}

async fn vector_search_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    payload: Result<Json<VectorSearchRequest>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => return vector_search_error(vector_json_rejection(&rejection)),
    };
    let username = match vector_api_username(
        &state,
        &headers,
        &request.vector_space,
        "search",
        &[(request.tenant_id, request.agent_id)],
    ) {
        Ok(username) => username,
        Err(msg) => {
            return vector_search_error(msg);
        }
    };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_search_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.search_timeout_ms,
        permit,
        async move { db.vector_search_as(username, request).await },
    )
    .await
    {
        Ok(result) => Json(VectorSearchResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            result: Some(result),
        })
        .into_response(),
        Err(error) => vector_search_error(error.to_string()),
    }
}

async fn vector_upsert_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    payload: Result<Json<VectorUpsertRequest>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => return vector_mutation_error(vector_json_rejection(&rejection)),
    };
    let scopes = request
        .items
        .iter()
        .map(|item| (item.tenant_id, item.agent_id))
        .collect::<Vec<_>>();
    let username =
        match vector_api_username(&state, &headers, &request.vector_space, "write", &scopes) {
            Ok(username) => username,
            Err(msg) => {
                return vector_mutation_error(msg);
            }
        };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_mutation_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.write_timeout_ms,
        permit,
        async move { db.vector_upsert_as(username, request).await },
    )
    .await
    {
        Ok(result) => Json(VectorMutationResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            result: Some(result),
        })
        .into_response(),
        Err(error) => vector_mutation_error(error.to_string()),
    }
}

async fn vector_delete_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    payload: Result<Json<VectorDeleteRequest>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => return vector_mutation_error(vector_json_rejection(&rejection)),
    };
    let scopes = request
        .items
        .iter()
        .map(|item| (item.tenant_id, item.agent_id))
        .collect::<Vec<_>>();
    let username =
        match vector_api_username(&state, &headers, &request.vector_space, "write", &scopes) {
            Ok(username) => username,
            Err(msg) => {
                return vector_mutation_error(msg);
            }
        };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_mutation_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.write_timeout_ms,
        permit,
        async move { db.vector_delete_as(username, request).await },
    )
    .await
    {
        Ok(result) => Json(VectorMutationResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            result: Some(result),
        })
        .into_response(),
        Err(error) => vector_mutation_error(error.to_string()),
    }
}

async fn vector_index_rebuild_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(vector_space): AxumPath<String>,
) -> Response {
    let Some(username) = session_username(&state, &headers) else {
        return vector_index_error("AUTHENTICATION_FAILED: authentication required".into());
    };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_index_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.maintenance_timeout_ms,
        permit,
        async move { db.rebuild_vector_index_as(username, vector_space).await },
    )
    .await
    {
        Ok(index) => Json(VectorIndexResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            index: Some(index),
        })
        .into_response(),
        Err(error) => vector_index_error(error.to_string()),
    }
}

async fn vector_index_compact_handler(
    State(state): State<WebState>,
    headers: HeaderMap,
    AxumPath(vector_space): AxumPath<String>,
) -> Response {
    let Some(username) = session_username(&state, &headers) else {
        return vector_index_error("AUTHENTICATION_FAILED: authentication required".into());
    };
    let permit = match acquire_vector_api_permit(&state) {
        Ok(permit) => permit,
        Err(error) => return vector_index_error(error),
    };
    let db = state.db.clone();
    match run_vector_request(
        state.vector_api_limits.maintenance_timeout_ms,
        permit,
        async move { db.compact_vector_index_as(username, vector_space).await },
    )
    .await
    {
        Ok(index) => Json(VectorIndexResp {
            success: true,
            error_code: None,
            trace_id: new_trace_id(),
            msg: "ok".into(),
            index: Some(index),
        })
        .into_response(),
        Err(error) => vector_index_error(error.to_string()),
    }
}

fn vector_index_error(msg: String) -> Response {
    let (status, error_code) = vector_error_contract(&msg);
    (
        status,
        Json(VectorIndexResp {
            success: false,
            error_code: Some(error_code),
            trace_id: new_trace_id(),
            msg,
            index: None,
        }),
    )
        .into_response()
}

fn vector_search_error(msg: String) -> Response {
    let (status, error_code) = vector_error_contract(&msg);
    (
        status,
        Json(VectorSearchResp {
            success: false,
            error_code: Some(error_code),
            trace_id: new_trace_id(),
            msg,
            result: None,
        }),
    )
        .into_response()
}

fn vector_mutation_error(msg: String) -> Response {
    let (status, error_code) = vector_error_contract(&msg);
    (
        status,
        Json(VectorMutationResp {
            success: false,
            error_code: Some(error_code),
            trace_id: new_trace_id(),
            msg,
            result: None,
        }),
    )
        .into_response()
}

fn acquire_vector_api_permit(state: &WebState) -> Result<OwnedSemaphorePermit, String> {
    state
        .vector_api_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| "RATE_LIMITED: vector API concurrency limit reached".to_string())
}

async fn run_vector_request<T>(
    timeout_ms: u64,
    permit: OwnedSemaphorePermit,
    request: impl Future<Output = crate::db::DbResult<T>> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let mut request = Box::pin(request);
    tokio::select! {
        result = &mut request => result.map_err(|error| error.to_string()),
        _ = time::sleep(Duration::from_millis(timeout_ms)) => {
            tokio::spawn(async move {
                let _permit = permit;
                let _ = request.await;
            });
            Err(format!(
                "REQUEST_TIMEOUT: vector API request exceeded {timeout_ms}ms; operation completion is unknown"
            ))
        }
    }
}

fn vector_json_rejection(rejection: &JsonRejection) -> String {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        "REQUEST_BODY_TOO_LARGE: vector API request body exceeds configured limit".into()
    } else {
        format!("INVALID_JSON: {}", rejection.body_text())
    }
}

fn vector_error_contract(msg: &str) -> (StatusCode, String) {
    let explicit = msg
        .split_once(':')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        });
    let code = explicit.unwrap_or_else(|| {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("permission denied") || lower.contains("not authorized") {
            "AUTHORIZATION_FAILED"
        } else if lower.contains("not found") || lower.contains("does not exist") {
            "VECTOR_SPACE_NOT_FOUND"
        } else if lower.contains("already exists") || lower.contains("duplicate") {
            "VECTOR_SPACE_ALREADY_EXISTS"
        } else if lower.contains("vss") || lower.contains("hnsw") {
            "VSS_UNAVAILABLE"
        } else if lower.contains("queue is full") {
            "RATE_LIMITED"
        } else if lower.contains("worker stopped") {
            "SERVICE_UNAVAILABLE"
        } else {
            "VECTOR_OPERATION_FAILED"
        }
    });
    let status = match code {
        "AUTHENTICATION_FAILED" => StatusCode::UNAUTHORIZED,
        "AUTHORIZATION_FAILED" | "TENANT_SCOPE_DENIED" => StatusCode::FORBIDDEN,
        "VECTOR_SPACE_NOT_FOUND" => StatusCode::NOT_FOUND,
        "VECTOR_SPACE_ALREADY_EXISTS"
        | "SOURCE_VERSION_CONFLICT"
        | "STALE_SOURCE_VERSION"
        | "DUPLICATE_VECTOR_KEY" => StatusCode::CONFLICT,
        "INDEX_BUILDING"
        | "INDEX_STALE"
        | "INDEX_UNAVAILABLE"
        | "VSS_UNAVAILABLE"
        | "SERVICE_UNAVAILABLE" => StatusCode::SERVICE_UNAVAILABLE,
        "RATE_LIMITED" => StatusCode::TOO_MANY_REQUESTS,
        "REQUEST_BODY_TOO_LARGE" => StatusCode::PAYLOAD_TOO_LARGE,
        "REQUEST_TIMEOUT" => StatusCode::GATEWAY_TIMEOUT,
        "VECTOR_OPERATION_FAILED" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, code.to_string())
}

fn resolve_parquet_import_sources(
    parquet_import_root: &Path,
    req: &ParquetImportReq,
) -> Result<Vec<ParquetImportSource>, String> {
    let source = req.source.trim();
    if source.is_empty() {
        return Err("Parquet import source path cannot be empty".into());
    }
    let relative = Path::new(source);
    if relative.is_absolute() {
        return Err("Parquet import source path must be relative to the configured root".into());
    }
    let root = fs::canonicalize(parquet_import_root).map_err(|e| {
        format!(
            "Parquet import root is unavailable: {}: {e}",
            parquet_import_root.display()
        )
    })?;
    let source_path = fs::canonicalize(root.join(relative))
        .map_err(|e| format!("Parquet import source is unavailable: {source}: {e}"))?;
    if !source_path.starts_with(&root) {
        return Err("Parquet import source escapes the configured root".into());
    }

    let metadata = fs::metadata(&source_path)
        .map_err(|e| format!("read Parquet import source metadata failed: {e}"))?;
    let mut files = if metadata.is_file() {
        if !is_parquet_file(&source_path) {
            return Err("Parquet import source file must use the .parquet extension".into());
        }
        vec![source_path]
    } else if metadata.is_dir() {
        if req.table.is_some() {
            return Err("target table can only be specified for a single Parquet file".into());
        }
        fs::read_dir(&source_path)
            .map_err(|e| format!("read Parquet import source directory failed: {e}"))?
            .map(|entry| entry.map(|value| value.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read Parquet import source entry failed: {e}"))?
            .into_iter()
            .filter(|path| path.is_file() && is_parquet_file(path))
            .collect::<Vec<_>>()
    } else {
        return Err("Parquet import source must be a Parquet file or directory".into());
    };
    files.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    if files.is_empty() {
        return Err("Parquet import source contains no .parquet files".into());
    }
    if files.len() > 256 {
        return Err("Parquet import supports at most 256 files per batch".into());
    }

    files
        .into_iter()
        .map(|path| {
            let canonical = fs::canonicalize(&path)
                .map_err(|e| format!("resolve Parquet source failed: {e}"))?;
            if !canonical.starts_with(&root) {
                return Err("Parquet source escapes the configured root".into());
            }
            let table = match &req.table {
                Some(table) => table.trim().to_string(),
                None => canonical
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "Parquet file name is not valid UTF-8".to_string())?
                    .to_string(),
            };
            Ok(ParquetImportSource {
                table,
                path: duckdb_path(&canonical),
            })
        })
        .collect()
}

fn is_parquet_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("parquet"))
}

fn duckdb_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
}

pub fn web_router(
    db: DbHandle,
    snapshot_dir: String,
    snapshot_prefix: String,
    parquet_import_root: String,
    vector_api_tokens: Vec<VectorApiTokenConfig>,
    vector_api_limits: VectorApiLimitsConfig,
) -> Router {
    validate_vector_api_limits(&vector_api_limits)
        .unwrap_or_else(|error| panic!("invalid web.vector_api_limits: {error}"));
    let vector_api_semaphore = Arc::new(Semaphore::new(vector_api_limits.max_concurrent_requests));
    let vector_body_limit = vector_api_limits.max_body_bytes;
    let vector_api_tokens = build_vector_service_tokens(vector_api_tokens)
        .unwrap_or_else(|error| panic!("invalid web.vector_api_tokens: {error}"));
    Router::new()
        .route("/", get(index_page))
        .route("/healthz", get(health_handler))
        .route("/assets/codemirror.bundle.js", get(codemirror_js))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .route("/session", get(session_handler))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
        .route(
            "/api/vector/indexes",
            post(vector_index_create_handler).layer(DefaultBodyLimit::max(vector_body_limit)),
        )
        .route(
            "/api/vector/search",
            post(vector_search_handler).layer(DefaultBodyLimit::max(vector_body_limit)),
        )
        .route(
            "/api/vector/upsert-batch",
            post(vector_upsert_handler).layer(DefaultBodyLimit::max(vector_body_limit)),
        )
        .route(
            "/api/vector/delete-batch",
            post(vector_delete_handler).layer(DefaultBodyLimit::max(vector_body_limit)),
        )
        .route(
            "/api/vector/indexes/{vector_space}/rebuild",
            post(vector_index_rebuild_handler),
        )
        .route(
            "/api/vector/indexes/{vector_space}/compact",
            post(vector_index_compact_handler),
        )
        .route(
            "/api/vector/indexes/{vector_space}/status",
            get(vector_index_status_handler),
        )
        .route(
            "/parquet-import",
            get(parquet_import_info_handler).post(parquet_import_handler),
        )
        .with_state(WebState {
            db,
            snapshot_dir: Arc::new(snapshot_dir),
            snapshot_prefix: Arc::new(snapshot_prefix),
            parquet_import_root: Arc::new(PathBuf::from(parquet_import_root)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            vector_api_tokens: Arc::new(vector_api_tokens),
            vector_api_limits: Arc::new(vector_api_limits),
            vector_api_semaphore,
        })
}

fn validate_vector_api_limits(config: &VectorApiLimitsConfig) -> Result<(), String> {
    if config.max_body_bytes == 0 {
        return Err("max_body_bytes must be greater than zero".into());
    }
    if config.max_concurrent_requests == 0 {
        return Err("max_concurrent_requests must be greater than zero".into());
    }
    if config.search_timeout_ms == 0
        || config.write_timeout_ms == 0
        || config.maintenance_timeout_ms == 0
    {
        return Err("all vector API timeouts must be greater than zero".into());
    }
    Ok(())
}

fn build_vector_service_tokens(
    configs: Vec<VectorApiTokenConfig>,
) -> Result<HashMap<String, VectorServiceIdentity>, String> {
    let mut tokens = HashMap::new();
    for config in configs {
        let token = config.token.trim();
        if token.len() < 32 {
            return Err("vector API token must contain at least 32 characters".into());
        }
        if config.username.trim().is_empty() {
            return Err("vector API token username cannot be empty".into());
        }
        if config.tenant_ids.is_empty() {
            return Err("vector API token must allow at least one tenant_id".into());
        }
        if config.vector_spaces.is_empty()
            || config
                .vector_spaces
                .iter()
                .any(|vector_space| vector_space.trim().is_empty())
        {
            return Err("vector API token must allow at least one non-empty vector_space".into());
        }
        if config.permissions.is_empty()
            || config
                .permissions
                .iter()
                .any(|permission| !matches!(permission.as_str(), "search" | "write"))
        {
            return Err("vector API token permissions must contain search and/or write".into());
        }
        let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
        if tokens
            .insert(
                digest,
                VectorServiceIdentity {
                    username: config.username,
                    tenant_ids: config.tenant_ids,
                    agent_ids: config.agent_ids,
                    vector_spaces: config.vector_spaces,
                    permissions: config.permissions,
                },
            )
            .is_some()
        {
            return Err("duplicate vector API token".into());
        }
    }
    Ok(tokens)
}

fn vector_api_username(
    state: &WebState,
    headers: &HeaderMap,
    vector_space: &str,
    permission: &str,
    scopes: &[(i64, i64)],
) -> Result<String, String> {
    if let Some(username) = session_username(state, headers) {
        return Ok(username);
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "AUTHENTICATION_FAILED: Bearer token is required".to_string())?;
    let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
    let identity = state
        .vector_api_tokens
        .get(&digest)
        .ok_or_else(|| "AUTHENTICATION_FAILED: invalid Bearer token".to_string())?;
    if !identity
        .vector_spaces
        .iter()
        .any(|allowed| allowed == vector_space)
        || !identity
            .permissions
            .iter()
            .any(|allowed| allowed == permission)
    {
        return Err(format!(
            "AUTHORIZATION_FAILED: vector_space={vector_space}, permission={permission}"
        ));
    }
    for (tenant_id, agent_id) in scopes {
        if !identity.tenant_ids.contains(tenant_id)
            || (!identity.agent_ids.is_empty() && !identity.agent_ids.contains(agent_id))
        {
            return Err(format!(
                "TENANT_SCOPE_DENIED: tenant_id={tenant_id}, agent_id={agent_id}"
            ));
        }
    }
    Ok(identity.username.clone())
}

fn session_username(state: &WebState, headers: &HeaderMap) -> Option<String> {
    let token = parse_session_token(headers)?;
    state.sessions.lock().ok()?.get(&token).cloned()
}

fn parse_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("rsduck_session=").map(str::to_string))
        .filter(|token| !token.is_empty())
}

fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

fn new_trace_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut trace_id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        trace_id.push_str(&format!("{byte:02x}"));
    }
    trace_id
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_vector_api_permit, build_vector_service_tokens, health_handler, paged_sql,
        parse_session_token, resolve_parquet_import_sources, run_vector_request, sql_handler,
        validate_vector_api_limits, vector_api_username, vector_error_contract, web_router,
        ParquetImportReq, SqlReq, WebState,
    };
    use crate::config::{DbConfig, VectorApiLimitsConfig, VectorApiTokenConfig};
    use crate::db::DbHandle;
    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::{header, HeaderMap, HeaderValue, Request, StatusCode},
        Json,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    #[test]
    fn parses_session_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=light; rsduck_session=abc123; other=1"),
        );

        assert_eq!(parse_session_token(&headers), Some("abc123".to_string()));
    }

    #[test]
    fn vector_bearer_token_enforces_tenant_and_agent_scope() {
        let token = "0123456789abcdef0123456789abcdef";
        let identities = build_vector_service_tokens(vec![VectorApiTokenConfig {
            token: token.into(),
            username: "agent_service".into(),
            tenant_ids: vec![1001],
            agent_ids: vec![2001],
            vector_spaces: vec!["agent-memory-v1".into()],
            permissions: vec!["search".into(), "write".into()],
        }])
        .unwrap();
        assert!(!identities.contains_key(token));

        let db = DbHandle::open(
            None,
            &DbConfig {
                vss_enabled: false,
                ..DbConfig::default()
            },
        );
        let state = WebState {
            db: db.clone(),
            snapshot_dir: Arc::new(String::new()),
            snapshot_prefix: Arc::new(String::new()),
            parquet_import_root: Arc::new(PathBuf::new()),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            vector_api_tokens: Arc::new(identities),
            vector_api_limits: Arc::new(VectorApiLimitsConfig::default()),
            vector_api_semaphore: Arc::new(Semaphore::new(1)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert_eq!(
            vector_api_username(
                &state,
                &headers,
                "agent-memory-v1",
                "search",
                &[(1001, 2001)]
            )
            .unwrap(),
            "agent_service"
        );
        assert!(vector_api_username(
            &state,
            &headers,
            "agent-memory-v1",
            "search",
            &[(1002, 2001)]
        )
        .unwrap_err()
        .contains("TENANT_SCOPE_DENIED"));
        assert!(vector_api_username(
            &state,
            &headers,
            "agent-memory-v1",
            "search",
            &[(1001, 2002)]
        )
        .unwrap_err()
        .contains("TENANT_SCOPE_DENIED"));
        assert!(
            vector_api_username(&state, &headers, "other-space", "search", &[(1001, 2001)])
                .unwrap_err()
                .contains("AUTHORIZATION_FAILED")
        );
        db.shutdown();
    }

    #[tokio::test]
    async fn web_sql_serializes_fixed_float_array_as_json() {
        let db = DbHandle::open(
            None,
            &DbConfig {
                vss_enabled: false,
                ..DbConfig::default()
            },
        );
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "test-session".to_string(),
            "admin".to_string(),
        )])));
        let state = WebState {
            db: db.clone(),
            snapshot_dir: Arc::new(String::new()),
            snapshot_prefix: Arc::new(String::new()),
            parquet_import_root: Arc::new(PathBuf::new()),
            sessions,
            vector_api_tokens: Arc::new(HashMap::new()),
            vector_api_limits: Arc::new(VectorApiLimitsConfig::default()),
            vector_api_semaphore: Arc::new(Semaphore::new(1)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("rsduck_session=test-session"),
        );

        let Json(response) = sql_handler(
            State(state),
            headers,
            Json(SqlReq {
                sql: "SELECT [0.125, 0.25, 0.5]::FLOAT[3] AS embedding".into(),
                page: 0,
                page_size: 100,
            }),
        )
        .await;

        assert!(response.success, "{}", response.msg);
        assert_eq!(response.columns[0].sql_type, "json");
        assert_eq!(response.columns[0].mysql_type, "json");
        assert_eq!(response.rows, vec![vec![Some("[0.125,0.25,0.5]".into())]]);
        db.shutdown();
    }

    #[tokio::test]
    async fn vector_api_limits_concurrency_and_timeout_are_explicit() {
        let db = DbHandle::open(
            None,
            &DbConfig {
                vss_enabled: false,
                ..DbConfig::default()
            },
        );
        let state = WebState {
            db: db.clone(),
            snapshot_dir: Arc::new(String::new()),
            snapshot_prefix: Arc::new(String::new()),
            parquet_import_root: Arc::new(PathBuf::new()),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            vector_api_tokens: Arc::new(HashMap::new()),
            vector_api_limits: Arc::new(VectorApiLimitsConfig::default()),
            vector_api_semaphore: Arc::new(Semaphore::new(1)),
        };
        let permit = acquire_vector_api_permit(&state).unwrap();
        assert!(acquire_vector_api_permit(&state)
            .unwrap_err()
            .contains("RATE_LIMITED"));
        drop(permit);
        assert!(acquire_vector_api_permit(&state).is_ok());

        let completion_semaphore = Arc::new(Semaphore::new(1));
        let completion_permit = completion_semaphore.clone().try_acquire_owned().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let timeout = run_vector_request(1, completion_permit, async move {
            release_rx.await.unwrap();
            Ok::<(), crate::db::DbError>(())
        })
        .await
        .unwrap_err();
        assert!(timeout.contains("REQUEST_TIMEOUT"));
        assert!(completion_semaphore.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        let released_permit = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            completion_semaphore.acquire_owned(),
        )
        .await
        .expect("background request did not release its concurrency permit")
        .expect("completion semaphore was closed");
        drop(released_permit);
        db.shutdown();
    }

    #[test]
    fn vector_api_limits_reject_zero_values() {
        let mut config = VectorApiLimitsConfig::default();
        validate_vector_api_limits(&config).unwrap();
        config.max_concurrent_requests = 0;
        assert!(validate_vector_api_limits(&config).is_err());
    }

    #[tokio::test]
    async fn vector_api_body_limit_returns_structured_error() {
        let db = DbHandle::open(
            None,
            &DbConfig {
                vss_enabled: false,
                ..DbConfig::default()
            },
        );
        let limits = VectorApiLimitsConfig {
            max_body_bytes: 64,
            ..VectorApiLimitsConfig::default()
        };
        let app = web_router(
            db.clone(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            limits,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/vector/search")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"vector_space":"space","embedding":"{}"}}"#,
                        "x".repeat(128)
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error_code"], "REQUEST_BODY_TOO_LARGE");
        assert_eq!(json["success"], false);
        db.shutdown();
    }

    #[test]
    fn vector_error_contract_exposes_stable_codes_and_http_statuses() {
        assert_eq!(
            vector_error_contract("VECTOR_DIMENSION_MISMATCH: expected=3, actual=2"),
            (
                axum::http::StatusCode::BAD_REQUEST,
                "VECTOR_DIMENSION_MISMATCH".into()
            )
        );
        assert_eq!(
            vector_error_contract("AUTHENTICATION_FAILED: invalid Bearer token"),
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "AUTHENTICATION_FAILED".into()
            )
        );
        assert_eq!(
            vector_error_contract("vector space memory does not exist"),
            (
                axum::http::StatusCode::NOT_FOUND,
                "VECTOR_SPACE_NOT_FOUND".into()
            )
        );
        assert_eq!(
            vector_error_contract("unexpected catalog error"),
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "VECTOR_OPERATION_FAILED".into()
            )
        );
        assert_eq!(
            vector_error_contract("REQUEST_TIMEOUT: exceeded 5000ms"),
            (StatusCode::GATEWAY_TIMEOUT, "REQUEST_TIMEOUT".into())
        );
        assert_eq!(
            vector_error_contract("REQUEST_BODY_TOO_LARGE: configured limit"),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "REQUEST_BODY_TOO_LARGE".into()
            )
        );
    }

    #[tokio::test]
    async fn health_endpoint_reports_running_version() {
        let Json(response) = health_handler().await;
        assert_eq!(response.status, "ok");
        assert_eq!(response.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn paged_sql_wraps_only_queries_without_top_level_paging() {
        assert_eq!(
            paged_sql("SELECT * FROM kline_day", 2, 100),
            "SELECT * FROM (SELECT * FROM kline_day) __rsduck_page LIMIT 100 OFFSET 200"
        );
        assert_eq!(
            paged_sql("SELECT * FROM kline_day LIMIT 100 OFFSET 200", 2, 100),
            "SELECT * FROM kline_day LIMIT 100 OFFSET 200"
        );
        assert_eq!(
            paged_sql("SELECT * FROM (SELECT * FROM kline_day LIMIT 10) t", 1, 50),
            "SELECT * FROM (SELECT * FROM (SELECT * FROM kline_day LIMIT 10) t) __rsduck_page LIMIT 50 OFFSET 50"
        );
        assert_eq!(paged_sql("SHOW TABLES", 1, 100), "SHOW TABLES");
    }

    #[test]
    fn parquet_import_directory_maps_each_file_to_one_table() {
        let temp = std::env::temp_dir().join(format!(
            "rsduck_web_parquet_import_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        let root = temp.join("root");
        let batch = root.join("batch");
        fs::create_dir_all(&batch).unwrap();
        fs::write(batch.join("alpha.parquet"), b"").unwrap();
        fs::write(batch.join("beta.PARQUET"), b"").unwrap();
        fs::write(batch.join("schema.sql"), b"").unwrap();

        let sources = resolve_parquet_import_sources(
            &root,
            &ParquetImportReq {
                source: "batch".into(),
                schema: "main".into(),
                table: None,
            },
        )
        .unwrap();

        assert_eq!(
            sources
                .iter()
                .map(|source| source.table.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn parquet_import_source_cannot_escape_configured_root() {
        let temp = std::env::temp_dir().join(format!(
            "rsduck_web_parquet_import_escape_{}_{}",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ));
        let root = temp.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(temp.join("outside.parquet"), b"").unwrap();

        let error = resolve_parquet_import_sources(
            &root,
            &ParquetImportReq {
                source: "../outside.parquet".into(),
                schema: "main".into(),
                table: None,
            },
        )
        .unwrap_err();

        assert!(error.contains("escapes the configured root"));
        let _ = fs::remove_dir_all(temp);
    }
}

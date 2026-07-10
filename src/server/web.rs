use axum::{
    extract::{Json, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::auth::{AuthProtocol, AuthRequest};
use crate::db::{DbHandle, ParquetImportSource, SqlTypedResult, SqlValue};

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

#[derive(Clone)]
pub struct WebState {
    pub db: DbHandle,
    pub snapshot_dir: Arc<String>,
    pub snapshot_prefix: Arc<String>,
    pub parquet_import_root: Arc<PathBuf>,
    pub sessions: Arc<Mutex<HashMap<String, String>>>,
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
) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/assets/codemirror.bundle.js", get(codemirror_js))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .route("/session", get(session_handler))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
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
        })
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

#[cfg(test)]
mod tests {
    use super::{paged_sql, parse_session_token, resolve_parquet_import_sources, ParquetImportReq};
    use axum::http::{header, HeaderMap, HeaderValue};
    use std::fs;

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

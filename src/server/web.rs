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
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::auth::{AuthProtocol, AuthRequest};
use crate::db::{DbHandle, SqlTypedResult, SqlValue};

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
    pub pg_type_oid: u32,
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

#[derive(Clone)]
pub struct WebState {
    pub db: DbHandle,
    pub snapshot_dir: Arc<String>,
    pub snapshot_prefix: Arc<String>,
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
                    pg_type_oid: column.data_type.pg_type_oid(),
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

pub fn web_router(db: DbHandle, snapshot_dir: String, snapshot_prefix: String) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/assets/codemirror.bundle.js", get(codemirror_js))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .route("/session", get(session_handler))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
        .with_state(WebState {
            db,
            snapshot_dir: Arc::new(snapshot_dir),
            snapshot_prefix: Arc::new(snapshot_prefix),
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
    use super::{paged_sql, parse_session_token};
    use axum::http::{header, HeaderMap, HeaderValue};

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
}

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

#[derive(Debug, Deserialize)]
pub struct SqlReq {
    pub sql: String,
    pub page: usize,
    pub page_size: usize,
}

#[derive(Debug, Serialize)]
pub struct SqlResp {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub success: bool,
    pub msg: String,
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

    match crate::db::execute_sql_as(username, sql).await {
        Ok(crate::db::SqlResult::Query { columns, rows }) => Json(SqlResp {
            columns,
            rows,
            success: true,
            msg: "ok".into(),
        }),
        Ok(crate::db::SqlResult::Execute {
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

    match crate::db::authenticate_user(username.clone(), req.password).await {
        Ok(()) => {
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
            msg: e,
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

    match crate::db::save_snapshot_as(
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

pub fn web_router(snapshot_dir: String, snapshot_prefix: String) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/assets/codemirror.bundle.js", get(codemirror_js))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .route("/session", get(session_handler))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
        .with_state(WebState {
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

const CODEMIRROR_JS: &str = include_str!("../../web/dist/codemirror.bundle.js");

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>rsduck console</title>
<style>
* { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body { font-family: "Segoe UI", Arial, sans-serif; color: #1f2933; background: #fff; overflow: hidden; }
button, input, textarea { font: inherit; }
.auth-screen { position: fixed; inset: 0; display: grid; place-items: center; background: #eef2f7; z-index: 20; }
.auth-screen[hidden] { display: none; }
.login-card { width: min(360px, calc(100vw - 32px)); padding: 26px; border: 1px solid #d4dbe6; border-radius: 8px; background: #fff; box-shadow: 0 16px 42px rgba(15, 23, 42, .14); }
.login-title { font-size: 20px; font-weight: 700; margin-bottom: 18px; }
.login-field { display: flex; flex-direction: column; gap: 6px; margin-bottom: 13px; color: #334155; font-size: 13px; }
.login-field input { height: 34px; padding: 5px 9px; border: 1px solid #b9c4d3; border-radius: 4px; background: #fff; }
.login-field input:focus { outline: 2px solid #8fc2ff; border-color: #1a73e8; }
.login-actions { display: flex; justify-content: flex-end; align-items: center; gap: 10px; margin-top: 18px; }
.login-error { min-height: 18px; color: #b42318; font-size: 13px; }
.app { display: grid; grid-template-columns: 300px minmax(0, 1fr); height: 100vh; }
.app[hidden] { display: none; }
.sidebar { min-width: 0; border-right: 1px solid #d8dde6; background: #f7f9fc; display: flex; flex-direction: column; }
.brand { height: 44px; display: flex; align-items: center; padding: 0 14px; font-weight: 700; border-bottom: 1px solid #d8dde6; background: #fff; }
.db-node { padding: 10px 12px 8px; border-bottom: 1px solid #e3e7ee; }
.db-title { font-size: 13px; font-weight: 600; display: flex; align-items: center; gap: 7px; }
.db-title::before { content: ""; width: 12px; height: 12px; border-radius: 3px; background: #0f9f6e; display: inline-block; }
.schema-tools { display: flex; gap: 6px; margin-top: 9px; }
.schema-tools input { min-width: 0; flex: 1; height: 28px; padding: 4px 8px; border: 1px solid #c8d0dc; border-radius: 4px; background: #fff; font-size: 12px; }
.icon-button { height: 28px; padding: 0 9px; border: 1px solid #b9c4d3; border-radius: 4px; background: #fff; color: #334155; cursor: pointer; font-size: 12px; }
.icon-button:hover { background: #edf4ff; border-color: #7daeea; }
.tree { overflow: auto; padding: 6px 0 12px; flex: 1; }
.schema-group { margin: 2px 0 8px; }
.schema-name { padding: 5px 12px; color: #526172; font-size: 11px; font-weight: 700; text-transform: uppercase; letter-spacing: .04em; }
.table-row { width: 100%; display: grid; grid-template-columns: 18px minmax(0, 1fr) auto; align-items: center; gap: 6px; border: 0; background: transparent; color: #1f2933; text-align: left; padding: 4px 10px 4px 18px; cursor: pointer; font-size: 13px; }
.table-row:hover { background: #e8f2ff; }
.table-row.active { background: #cfe6ff; outline: 1px solid #59a8ff; outline-offset: -1px; }
.table-icon { width: 13px; height: 12px; border: 1px solid #5aa2d8; background: linear-gradient(#9fd4ff 0 35%, #4da3df 35% 100%); border-radius: 2px; }
.table-name { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
.table-meta { color: #708090; font-size: 11px; }
.empty { color: #66758a; padding: 14px; font-size: 13px; }
.main { --editor-height: 38vh; min-width: 0; display: grid; grid-template-rows: 44px minmax(150px, var(--editor-height)) 8px auto minmax(0, 1fr); height: 100vh; }
.topbar { display: flex; align-items: center; justify-content: space-between; gap: 12px; height: 44px; padding: 0 14px; border-bottom: 1px solid #d8dde6; background: #f9fafb; }
.title { font-weight: 700; }
.top-actions { display: flex; align-items: center; gap: 10px; min-width: 0; }
.summary { color: #66758a; font-size: 12px; white-space: nowrap; }
.user-pill { color: #334155; font-size: 12px; white-space: nowrap; }
.editor { min-height: 0; display: flex; flex-direction: column; border-bottom: 1px solid #d8dde6; }
.editor-surface { flex: 1; min-height: 130px; background: #fff; overflow: hidden; }
.editor-surface .cm-editor { height: 100%; }
.editor-surface .cm-focused { outline: none; }
.editor-surface .cm-selectionBackground { background: #7fb3ff !important; }
.editor-surface .cm-line::selection,
.editor-surface .cm-line *::selection,
.editor-surface .cm-content::selection,
.editor-surface .cm-content *::selection { background: #7fb3ff !important; color: inherit !important; }
.toolbar { display: flex; align-items: center; gap: 8px; padding: 8px 12px; border-top: 1px solid #e6e9ef; background: #f4f6f8; }
.context-menu { position: fixed; z-index: 1000; min-width: 150px; padding: 4px; border: 1px solid #b9c4d3; border-radius: 4px; background: #fff; box-shadow: 0 8px 24px rgba(15, 23, 42, .18); }
.context-menu[hidden] { display: none; }
.context-menu button { width: 100%; height: 30px; border: 0; border-radius: 3px; background: transparent; color: #1f2933; text-align: left; padding: 0 10px; cursor: pointer; }
.context-menu button:hover { background: #edf4ff; color: #0b61c9; }
.splitter { height: 8px; cursor: row-resize; background: #f4f6f8; border-top: 1px solid #d8dde6; border-bottom: 1px solid #d8dde6; position: relative; }
.splitter::before { content: ""; position: absolute; left: 12px; right: 12px; top: 3px; height: 2px; border-radius: 1px; background: #b8c2d1; }
.splitter:hover, .splitter.dragging { background: #eaf2ff; }
.splitter:hover::before, .splitter.dragging::before { background: #5b9ee8; }
body.resizing { cursor: row-resize; user-select: none; }
.primary-button { height: 30px; padding: 0 14px; border: 1px solid #1565c0; border-radius: 4px; color: #fff; background: #1a73e8; cursor: pointer; }
.primary-button:hover { background: #1558b0; }
.secondary-button { height: 30px; padding: 0 12px; border: 1px solid #b9c4d3; border-radius: 4px; color: #263445; background: #fff; cursor: pointer; }
.secondary-button:hover { background: #edf4ff; border-color: #7daeea; }
.pager { display: flex; gap: 8px; align-items: center; padding: 7px 12px; border-bottom: 1px solid #d8dde6; background: #fff; color: #475569; font-size: 13px; }
.pager input { width: 96px; height: 28px; padding: 4px 7px; border: 1px solid #c8d0dc; border-radius: 4px; }
.status { margin-left: auto; color: #66758a; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
.result { min-width: 0; min-height: 0; overflow: auto; background: #fff; }
table { width: max-content; min-width: 100%; border-collapse: collapse; font-size: 13px; }
th, td { border-right: 1px solid #e1e5eb; border-bottom: 1px solid #e1e5eb; padding: 6px 9px; text-align: left; white-space: nowrap; }
th { position: sticky; top: 0; background: #f3f5f7; color: #111827; z-index: 1; }
td { color: #253244; }
tbody tr:nth-child(even) { background: #fafbfc; }
@media (max-width: 760px) {
  body { overflow: auto; }
  .app { grid-template-columns: 1fr; grid-template-rows: 240px 1fr; height: auto; min-height: 100vh; }
  .sidebar { border-right: 0; border-bottom: 1px solid #d8dde6; }
  .main { height: calc(100vh - 240px); min-height: 520px; }
}
</style>
</head>
<body>
<div id="authScreen" class="auth-screen">
  <form class="login-card" onsubmit="login(event)">
    <div class="login-title">rsduck</div>
    <label class="login-field">
      <span>Username</span>
      <input id="loginUsername" autocomplete="username" autofocus>
    </label>
    <label class="login-field">
      <span>Password</span>
      <input id="loginPassword" type="password" autocomplete="current-password">
    </label>
    <div id="loginMsg" class="login-error"></div>
    <div class="login-actions">
      <button class="primary-button" type="submit">Sign in</button>
    </div>
  </form>
</div>
<div id="app" class="app" hidden>
  <aside class="sidebar">
    <div class="brand">rsduck</div>
    <div class="db-node">
      <div class="db-title">memory</div>
      <div class="schema-tools">
        <input id="tableFilter" placeholder="Filter tables" oninput="renderTables()">
        <button class="icon-button" onclick="loadTables()" title="Refresh tables">Refresh</button>
      </div>
    </div>
    <div id="tableList" class="tree"><div class="empty">Loading tables...</div></div>
  </aside>
  <main class="main">
    <div class="topbar">
      <div class="title">SQL Console</div>
      <div class="top-actions">
        <div id="schemaSummary" class="summary">0 tables</div>
        <span id="currentUser" class="user-pill"></span>
        <button class="secondary-button" onclick="saveSnapshot()">Save Snapshot</button>
        <button class="secondary-button" onclick="logout()">Logout</button>
      </div>
    </div>
    <section class="editor">
      <div id="sqlEditor" class="editor-surface"></div>
      <div class="toolbar">
        <button class="primary-button" onclick="run()">Execute</button>
      </div>
    </section>
    <div id="editorSplitter" class="splitter" title="Drag to resize editor"></div>
    <div class="pager">
      <button class="secondary-button" onclick="prevPage()">Prev</button>
      <span id="pageLabel">Page 1</span>
      <button class="secondary-button" onclick="nextPage()">Next</button>
      <span>Page size</span>
      <input id="pageSize" type="number" min="1" max="100000" value="100">
      <span id="msg" class="status"></span>
    </div>
    <section class="result">
      <table id="tbl"></table>
    </section>
  </main>
</div>
<div id="sqlContextMenu" class="context-menu" hidden>
  <button type="button" onclick="executeContextSql()">Execute Selection</button>
</div>
<script src="/assets/codemirror.bundle.js"></script>
<script>
let currentPage = 0;
let lastSql = '';
let tables = [];
let activeTable = '';
let contextSql = '';
let sqlEditor = null;
let currentUser = '';
let uiReady = false;
const editorHeightKey = 'rsduck.editorHeight';

function escapeHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, ch => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;'
  }[ch]));
}

function ensureUiReady() {
  if (uiReady) return;
  setupSqlEditor();
  setupEditorSplitter();
  uiReady = true;
}

function showLogin(message = '') {
  document.getElementById('authScreen').hidden = false;
  document.getElementById('app').hidden = true;
  document.getElementById('loginMsg').innerText = message;
  setTimeout(() => document.getElementById('loginUsername').focus(), 0);
}

function showApp(username) {
  currentUser = username;
  document.getElementById('currentUser').innerText = username;
  document.getElementById('authScreen').hidden = true;
  document.getElementById('app').hidden = false;
  ensureUiReady();
}

async function login(event) {
  event.preventDefault();
  const username = document.getElementById('loginUsername').value.trim();
  const password = document.getElementById('loginPassword').value;
  const msg = document.getElementById('loginMsg');
  msg.innerText = '';
  const resp = await fetch('/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password })
  });
  const data = await resp.json();
  if (!data.success) {
    msg.innerText = data.msg || 'login failed';
    return;
  }
  document.getElementById('loginPassword').value = '';
  showApp(data.username);
  await loadTables();
  await run();
}

async function logout() {
  await fetch('/logout', { method: 'POST' });
  tables = [];
  activeTable = '';
  document.getElementById('tbl').innerHTML = '';
  document.getElementById('msg').innerText = '';
  showLogin();
}

async function initSession() {
  const resp = await fetch('/session');
  const data = await resp.json();
  if (!data.authenticated) {
    showLogin();
    return;
  }
  showApp(data.username);
  await loadTables();
  await run();
}

function setSqlValue(value) {
  sqlEditor.setValue(value);
}

function getSqlValue() {
  return sqlEditor ? sqlEditor.getValue() : '';
}

function getSelectedSql() {
  return sqlEditor ? sqlEditor.getSelectedText() : '';
}

function showSqlContextMenu(x, y) {
  const menu = document.getElementById('sqlContextMenu');
  menu.hidden = false;
  const left = Math.min(x, window.innerWidth - menu.offsetWidth - 8);
  const top = Math.min(y, window.innerHeight - menu.offsetHeight - 8);
  menu.style.left = Math.max(8, left) + 'px';
  menu.style.top = Math.max(8, top) + 'px';
}

function hideSqlContextMenu() {
  document.getElementById('sqlContextMenu').hidden = true;
}

function executeContextSql() {
  const sql = contextSql;
  hideSqlContextMenu();
  if (sql) runSqlText(sql, true);
}

function setupSqlEditor() {
  const parent = document.getElementById('sqlEditor');
  sqlEditor = window.RsduckEditor.create({
    parent,
    initialDoc: 'SHOW TABLES;',
    onRun: sql => runSqlText(sql, true)
  });

  parent.addEventListener('contextmenu', event => {
    const sql = getSelectedSql();
    if (!sql) {
      hideSqlContextMenu();
      return;
    }
    event.preventDefault();
    contextSql = sql;
    showSqlContextMenu(event.clientX, event.clientY);
  });

  document.addEventListener('click', hideSqlContextMenu);
  window.addEventListener('resize', hideSqlContextMenu);
}

function quoteIdent(value) {
  return '"' + String(value).replace(/"/g, '""') + '"';
}

function tableSql(schema, table) {
  return 'SELECT * FROM ' + quoteIdent(schema) + '.' + quoteIdent(table) + ' LIMIT 100;';
}

function shouldRefreshTables(sql) {
  const cleanSql = sql
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/--.*$/gm, ' ')
    .trim();
  const command = cleanSql.split(/\s+/)[0]?.toUpperCase() || '';
  return ['CREATE', 'DROP', 'ALTER', 'IMPORT', 'ATTACH', 'DETACH'].includes(command);
}

async function postSql(sql, page = 0, pageSize = 1000) {
  const resp = await fetch('/sql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sql, page, page_size: pageSize })
  });
  return await resp.json();
}

async function loadTables(showErrors = true) {
  const sql = `
    SELECT table_schema, table_name, table_type
    FROM information_schema.tables
    WHERE table_schema NOT IN ('pg_catalog', 'information_schema', 'rsduck_catalog', 'rsduck_internal')
    ORDER BY table_schema, table_name
  `;
  const data = await postSql(sql, 0, 10000);
  if (!data.success) {
    if (showErrors) {
      document.getElementById('tableList').innerHTML =
        '<div class="empty">Failed to load tables: ' + escapeHtml(data.msg) + '</div>';
    }
    return;
  }

  const schemaIdx = data.columns.findIndex(c => c.toLowerCase() === 'table_schema');
  const tableIdx = data.columns.findIndex(c => c.toLowerCase() === 'table_name');
  const typeIdx = data.columns.findIndex(c => c.toLowerCase() === 'table_type');

  tables = data.rows.map(row => ({
    schema: row[schemaIdx] || 'main',
    name: row[tableIdx] || '',
    type: typeIdx >= 0 ? row[typeIdx] : ''
  })).filter(item => item.name);

  renderTables();
}

function renderTables() {
  const list = document.getElementById('tableList');
  const summary = document.getElementById('schemaSummary');
  const filter = document.getElementById('tableFilter').value.trim().toLowerCase();
  const visible = tables.filter(item =>
    !filter ||
    item.name.toLowerCase().includes(filter) ||
    item.schema.toLowerCase().includes(filter)
  );

  summary.innerText = tables.length + (tables.length === 1 ? ' table' : ' tables');
  if (!visible.length) {
    list.innerHTML = '<div class="empty">No tables</div>';
    return;
  }

  const groups = new Map();
  for (const item of visible) {
    if (!groups.has(item.schema)) groups.set(item.schema, []);
    groups.get(item.schema).push(item);
  }

  let html = '';
  for (const [schema, items] of groups) {
    html += '<div class="schema-group">';
    html += '<div class="schema-name">' + escapeHtml(schema) + '</div>';
    for (const item of items) {
      const key = item.schema + '.' + item.name;
      const meta = item.type || '';
      html += '<button class="table-row ' + (key === activeTable ? 'active' : '') + '" ';
      html += 'title="' + escapeHtml(key) + '" ';
      html += 'onclick="selectTable(' + escapeHtml(JSON.stringify(item.schema)) + ',' + escapeHtml(JSON.stringify(item.name)) + ')">';
      html += '<span class="table-icon"></span>';
      html += '<span class="table-name">' + escapeHtml(item.name) + '</span>';
      html += '<span class="table-meta">' + escapeHtml(meta) + '</span>';
      html += '</button>';
    }
    html += '</div>';
  }
  list.innerHTML = html;
}

function selectTable(schema, table) {
  activeTable = schema + '.' + table;
  setSqlValue(tableSql(schema, table));
  renderTables();
  run();
}

async function run(resetPage = true) {
  const sql = (getSelectedSql() || getSqlValue()).trim();
  return runSqlText(sql, resetPage);
}

async function runSqlText(sql, resetPage = true) {
  sql = sql.trim();
  if (!sql) return;
  if (resetPage || sql !== lastSql) currentPage = 0;
  lastSql = sql;
  const pageSize = Math.max(1, Math.min(100000, Number(document.getElementById('pageSize').value) || 100));
  const t0 = performance.now();
  const data = await postSql(sql, currentPage, pageSize);
  const ms = (performance.now() - t0).toFixed(1);
  const msg = document.getElementById('msg');
  if (!data.success) {
    msg.innerText = 'Error: ' + data.msg;
    return;
  }
  document.getElementById('pageLabel').innerText = 'Page ' + (currentPage + 1);
  msg.innerText = data.msg + ' in ' + ms + 'ms';
  const tbl = document.getElementById('tbl');
  if (!data.columns.length) {
    tbl.innerHTML = '';
    return;
  }
  let html = '<thead><tr>' + data.columns.map(c => '<th>' + escapeHtml(c) + '</th>').join('') + '</tr></thead><tbody>';
  for (const row of data.rows) {
    html += '<tr>' + row.map(v => '<td>' + escapeHtml(v) + '</td>').join('') + '</tr>';
  }
  html += '</tbody>';
  tbl.innerHTML = html;
  if (shouldRefreshTables(sql)) loadTables(false);
}

function nextPage() {
  currentPage += 1;
  run(false);
}

function prevPage() {
  if (currentPage === 0) return;
  currentPage -= 1;
  run(false);
}

async function saveSnapshot() {
  const t0 = performance.now();
  const resp = await fetch('/snapshot', { method: 'POST' });
  const data = await resp.json();
  const ms = (performance.now() - t0).toFixed(1);
  const msg = document.getElementById('msg');
  msg.innerText = (data.success ? data.msg : 'Error: ' + data.msg) + ' in ' + ms + 'ms';
}

function setEditorHeight(height) {
  const main = document.querySelector('.main');
  const bounds = main.getBoundingClientRect();
  const topbar = document.querySelector('.topbar').offsetHeight;
  const pager = document.querySelector('.pager').offsetHeight;
  const splitter = document.getElementById('editorSplitter').offsetHeight;
  const minEditor = 150;
  const minResult = 160;
  const maxEditor = Math.max(minEditor, bounds.height - topbar - pager - splitter - minResult);
  const nextHeight = Math.max(minEditor, Math.min(maxEditor, height));
  main.style.setProperty('--editor-height', nextHeight + 'px');
  localStorage.setItem(editorHeightKey, String(Math.round(nextHeight)));
}

function setupEditorSplitter() {
  const main = document.querySelector('.main');
  const splitter = document.getElementById('editorSplitter');
  const savedHeight = Number(localStorage.getItem(editorHeightKey));
  if (Number.isFinite(savedHeight) && savedHeight > 0) {
    setEditorHeight(savedHeight);
  }

  splitter.addEventListener('pointerdown', event => {
    event.preventDefault();
    splitter.setPointerCapture(event.pointerId);
    splitter.classList.add('dragging');
    document.body.classList.add('resizing');

    const onMove = moveEvent => {
      const bounds = main.getBoundingClientRect();
      const topbar = document.querySelector('.topbar').offsetHeight;
      setEditorHeight(moveEvent.clientY - bounds.top - topbar);
    };

    const onUp = upEvent => {
      splitter.releasePointerCapture(upEvent.pointerId);
      splitter.classList.remove('dragging');
      document.body.classList.remove('resizing');
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
    };

    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
  });

  window.addEventListener('resize', () => {
    const currentHeight = document.querySelector('.editor').getBoundingClientRect().height;
    setEditorHeight(currentHeight);
  });
}

initSession();
</script>
</body>
</html>
"#;

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

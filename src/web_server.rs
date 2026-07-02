use axum::{
    extract::{Json, State},
    response::Html,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

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

#[derive(Clone)]
pub struct WebState {
    pub pg_client: Arc<Client>,
    pub snapshot_dir: Arc<String>,
    pub snapshot_prefix: Arc<String>,
}

pub async fn create_pg_client(pg_bind: &str) -> Result<Client, tokio_postgres::Error> {
    let (host, port) = pg_bind
        .rsplit_once(':')
        .expect("pg bind must be host:port format");
    let mut config = tokio_postgres::Config::new();
    config
        .host(host)
        .port(port.parse().expect("invalid pg port"));

    let (client, connection) = config.connect(NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!("web console pg connection lost: {e}");
        }
    });

    Ok(client)
}

async fn sql_handler(State(state): State<WebState>, Json(req): Json<SqlReq>) -> Json<SqlResp> {
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

    match state.pg_client.simple_query(&sql).await {
        Ok(messages) => {
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let mut last_msg = String::from("ok");

            for msg in messages {
                match msg {
                    SimpleQueryMessage::Row(row) => {
                        if columns.is_empty() {
                            columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                        }
                        let line = (0..row.len())
                            .map(|i| row.get(i).unwrap_or("").to_string())
                            .collect();
                        rows.push(line);
                    }
                    SimpleQueryMessage::CommandComplete(affected) => {
                        last_msg = format!("{affected} row(s)");
                    }
                    _ => {}
                }
            }

            Json(SqlResp {
                columns,
                rows,
                success: true,
                msg: last_msg,
            })
        }
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
    if !is_pageable_sql(sql) {
        return sql.to_string();
    }

    let page_size = page_size.clamp(1, 100_000);
    let offset = page.saturating_mul(page_size);
    format!("SELECT * FROM ({sql}) __rsduck_page LIMIT {page_size} OFFSET {offset}")
}

fn is_pageable_sql(sql: &str) -> bool {
    let command = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        command.as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "EXPLAIN" | "PRAGMA"
    )
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn snapshot_handler(State(state): State<WebState>) -> Json<SqlResp> {
    let t0 = Instant::now();

    match crate::db::save_snapshot(state.snapshot_dir.as_str(), state.snapshot_prefix.as_str())
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

pub fn web_router(pg_client: Arc<Client>, snapshot_dir: String, snapshot_prefix: String) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
        .with_state(WebState {
            pg_client,
            snapshot_dir: Arc::new(snapshot_dir),
            snapshot_prefix: Arc::new(snapshot_prefix),
        })
}

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
.app { display: grid; grid-template-columns: 300px minmax(0, 1fr); height: 100vh; }
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
.editor { min-height: 0; display: flex; flex-direction: column; border-bottom: 1px solid #d8dde6; }
.editor textarea { flex: 1; width: 100%; min-height: 130px; resize: none; border: 0; outline: 0; padding: 12px 14px; font-family: Consolas, "Courier New", monospace; font-size: 14px; line-height: 1.55; color: #0f172a; background: #fff; }
.toolbar { display: flex; align-items: center; gap: 8px; padding: 8px 12px; border-top: 1px solid #e6e9ef; background: #f4f6f8; }
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
<div class="app">
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
        <button class="secondary-button" onclick="saveSnapshot()">Save Snapshot</button>
      </div>
    </div>
    <section class="editor">
      <textarea id="sql" spellcheck="false">SHOW TABLES;</textarea>
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
<script>
let currentPage = 0;
let lastSql = '';
let tables = [];
let activeTable = '';
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

function quoteIdent(value) {
  return '"' + String(value).replace(/"/g, '""') + '"';
}

function tableSql(schema, table) {
  return 'SELECT * FROM ' + quoteIdent(schema) + '.' + quoteIdent(table) + ' LIMIT 100;';
}

function shouldRefreshTables(sql) {
  const command = sql.trim().split(/\s+/)[0]?.toUpperCase() || '';
  return ['CREATE', 'DROP', 'ALTER', 'IMPORT'].includes(command);
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
    SELECT schema_name, table_name, column_count, estimated_size
    FROM duckdb_tables()
    WHERE internal = false
    ORDER BY schema_name, table_name
  `;
  const data = await postSql(sql, 0, 10000);
  if (!data.success) {
    if (showErrors) {
      document.getElementById('tableList').innerHTML =
        '<div class="empty">Failed to load tables: ' + escapeHtml(data.msg) + '</div>';
    }
    return;
  }

  const schemaIdx = data.columns.findIndex(c => c.toLowerCase() === 'schema_name');
  const tableIdx = data.columns.findIndex(c => c.toLowerCase() === 'table_name');
  const columnIdx = data.columns.findIndex(c => c.toLowerCase() === 'column_count');
  const sizeIdx = data.columns.findIndex(c => c.toLowerCase() === 'estimated_size');

  tables = data.rows.map(row => ({
    schema: row[schemaIdx] || 'main',
    name: row[tableIdx] || '',
    columns: columnIdx >= 0 ? row[columnIdx] : '',
    size: sizeIdx >= 0 ? row[sizeIdx] : ''
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
      const meta = item.columns ? item.columns + ' cols' : '';
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
  document.getElementById('sql').value = tableSql(schema, table);
  renderTables();
  run();
}

async function run(resetPage = true) {
  const sql = document.getElementById('sql').value.trim();
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

setupEditorSplitter();
loadTables().then(() => run());
</script>
</body>
</html>
"#;

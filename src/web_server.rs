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

    match crate::db::save_snapshot(state.snapshot_dir.as_str()).await {
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

pub fn web_router(pg_client: Arc<Client>, snapshot_dir: String) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/sql", post(sql_handler))
        .route("/snapshot", post(snapshot_handler))
        .with_state(WebState {
            pg_client,
            snapshot_dir: Arc::new(snapshot_dir),
        })
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>rsduck console</title>
<style>
body { font-family: "Segoe UI", sans-serif; max-width: 960px; margin: 40px auto; padding: 0 20px; }
h2 { color: #333; }
textarea { width: 100%; font-family: Consolas, monospace; font-size: 14px; padding: 10px; border: 1px solid #ccc; border-radius: 4px; resize: vertical; }
button { margin-top: 10px; padding: 8px 24px; font-size: 14px; cursor: pointer; background: #1a73e8; color: #fff; border: none; border-radius: 4px; }
button:hover { background: #1558b0; }
.toolbar { display: flex; gap: 8px; align-items: center; }
.pager { display: flex; gap: 8px; align-items: center; margin-top: 8px; color: #555; font-size: 13px; }
.pager input { width: 88px; padding: 5px 6px; border: 1px solid #ccc; border-radius: 4px; }
.pager button { margin-top: 0; padding: 5px 12px; }
#msg { margin-top: 8px; color: #666; font-size: 13px; }
table { width: 100%; border-collapse: collapse; margin-top: 16px; font-size: 13px; }
th, td { border: 1px solid #ddd; padding: 6px 10px; text-align: left; }
th { background: #f5f5f5; }
</style>
</head>
<body>
<h2>rsduck SQL Console</h2>
<textarea id="sql" rows="6">SELECT * FROM kline_day LIMIT 20;</textarea>
<div class="toolbar">
  <button onclick="run()">Execute</button>
  <button onclick="saveSnapshot()">Save Snapshot</button>
</div>
<div class="pager">
  <button onclick="prevPage()">Prev</button>
  <span id="pageLabel">Page 1</span>
  <button onclick="nextPage()">Next</button>
  <span>Page size</span>
  <input id="pageSize" type="number" min="1" max="100000" value="100">
</div>
<div id="msg"></div>
<table id="tbl"></table>
<script>
let currentPage = 0;
let lastSql = '';

async function run(resetPage = true) {
  const sql = document.getElementById('sql').value.trim();
  if (!sql) return;
  if (resetPage || sql !== lastSql) currentPage = 0;
  lastSql = sql;
  const pageSize = Math.max(1, Math.min(100000, Number(document.getElementById('pageSize').value) || 100));
  const t0 = performance.now();
  const resp = await fetch('/sql', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sql, page: currentPage, page_size: pageSize })
  });
  const data = await resp.json();
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
  let html = '<thead><tr>' + data.columns.map(c => '<th>' + c + '</th>').join('') + '</tr></thead><tbody>';
  for (const row of data.rows) {
    html += '<tr>' + row.map(v => '<td>' + (v ?? '') + '</td>').join('') + '</tr>';
  }
  html += '</tbody>';
  tbl.innerHTML = html;
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
</script>
</body>
</html>
"#;

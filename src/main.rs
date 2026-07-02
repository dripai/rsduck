mod config;
mod db;
mod pg_server;
mod web_server;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener as TokioTcpListener;
use tokio::time;
use tracing::{error, info, warn};

fn cleanup_old_snapshots(base_dir: &str, table_prefix: &str, retain_hours: u64) {
    let base = Path::new(base_dir);
    if !base.exists() {
        return;
    }

    let cutoff = chrono::Local::now() - chrono::Duration::hours(retain_hours as i64);
    let prefix = format!("{table_prefix}_");

    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) || !name.ends_with(".parquet") {
                continue;
            }

            let maybe_ts = name[prefix.len()..].strip_suffix(".parquet");
            if let Some(ts_part) = maybe_ts {
                if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(ts_part, "%Y%m%d_%H%M%S") {
                    let ts_local = ts
                        .and_local_timezone(chrono::Local)
                        .earliest()
                        .unwrap_or_else(|| ts.and_utc().with_timezone(&chrono::Local));
                    if ts_local < cutoff {
                        info!("Removing expired snapshot: {}", name);
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .init();

    let cfg = config::load_config();
    info!("Config loaded");

    let snapshot_file = if cfg.snapshot.restore_on_startup {
        db::find_latest_snapshot(&cfg.snapshot.dir, "kline_day")
    } else {
        None
    };
    db::init_db(snapshot_file.as_deref());
    info!("In-memory DuckDB initialized");

    let pg_bind = cfg.pg.bind.clone();
    let pg_task = tokio::spawn(async move {
        pg_server::start_pg_server(&pg_bind).await;
    });

    let pg_bind_for_web = cfg.pg.bind.clone();
    let pg_client = loop {
        match web_server::create_pg_client(&pg_bind_for_web).await {
            Ok(client) => break client,
            Err(e) => {
                warn!("Waiting for pg_server... ({e})");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    info!("Web console PG client connected to {}", pg_bind_for_web);

    let snap_dir = cfg.snapshot.dir.clone();
    let interval = cfg.snapshot.interval_secs;
    let retain = cfg.snapshot.retain_hours;
    let snapshot_task = tokio::spawn(async move {
        let mut ticker = time::interval(Duration::from_secs(interval));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let t0 = Instant::now();
            match db::save_snapshot(&snap_dir).await {
                Ok(path) => info!("Snapshot saved to {} ({:.2?})", path, t0.elapsed()),
                Err(e) => error!("Snapshot failed ({:.2?}): {}", t0.elapsed(), e),
            }
            cleanup_old_snapshots(&snap_dir, "kline_day", retain);
        }
    });

    let app = web_server::web_router(Arc::new(pg_client), cfg.snapshot.dir.clone());
    let shutdown_snapshot_dir = cfg.snapshot.dir.clone();
    let listener = TokioTcpListener::bind(&cfg.web.bind)
        .await
        .expect("bind web server failed");
    info!("Web console on http://{}", cfg.web.bind);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Listen shutdown signal failed: {}", e);
                return;
            }

            info!("Shutdown signal received, saving snapshot before exit");
            let t0 = Instant::now();
            match db::save_snapshot(&shutdown_snapshot_dir).await {
                Ok(path) => info!("Shutdown snapshot saved to {} ({:.2?})", path, t0.elapsed()),
                Err(e) => error!("Shutdown snapshot failed ({:.2?}): {}", t0.elapsed(), e),
            }
        })
        .await
        .expect("web server error");

    snapshot_task.abort();
    pg_task.abort();
}

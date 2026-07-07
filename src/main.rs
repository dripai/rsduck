mod catalog;
mod config;
mod db;
mod pg_compat;
mod server;
mod sql_route;

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::net::TcpListener as TokioTcpListener;
use tokio::time;
use tracing::{error, info};
use tracing_subscriber::filter::LevelFilter;

fn parse_log_level(level: &str) -> LevelFilter {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" | "warning" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "off" => LevelFilter::OFF,
        _ => panic!(
            "invalid log_level in rsduck.toml: {level}; expected trace, debug, info, warn, error, or off"
        ),
    }
}

fn cleanup_old_snapshots(base_dir: &str, snapshot_prefix: &str, retain_hours: u64) {
    let base = Path::new(base_dir);
    if !base.exists() {
        return;
    }

    let cutoff = chrono::Local::now() - chrono::Duration::hours(retain_hours as i64);

    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(ts) = db::parse_snapshot_dir_timestamp(&name, snapshot_prefix) {
                let ts_local = ts
                    .and_local_timezone(chrono::Local)
                    .earliest()
                    .unwrap_or_else(|| ts.and_utc().with_timezone(&chrono::Local));
                if ts_local < cutoff {
                    info!("Removing expired snapshot: {}", name);
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let cfg = config::load_config();
    tracing_subscriber::fmt()
        .with_max_level(parse_log_level(&cfg.log_level))
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .init();

    info!("Config loaded");
    db::validate_snapshot_prefix(&cfg.snapshot.prefix)
        .unwrap_or_else(|e| panic!("invalid snapshot prefix: {e}"));

    let snapshot_dir = if cfg.snapshot.restore_on_startup {
        db::find_latest_snapshot_dir(&cfg.snapshot.dir, &cfg.snapshot.prefix)
    } else {
        None
    };
    db::init_db(snapshot_dir.as_deref(), &cfg.db);
    info!("In-memory DuckDB initialized");

    let partition_task = if cfg.partition.maintenance_enabled {
        let interval = cfg.partition.maintenance_interval_secs.max(1);
        let _verify_interval = cfg.partition.verify_interval_secs.max(1);
        let _max_jobs_per_tick = cfg.partition.max_jobs_per_tick.max(1);
        Some(tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(interval));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let t0 = Instant::now();
                match db::run_partition_maintenance().await {
                    Ok(_) => info!("Partition maintenance completed ({:.2?})", t0.elapsed()),
                    Err(e) => error!("Partition maintenance failed ({:.2?}): {}", t0.elapsed(), e),
                }
            }
        }))
    } else {
        None
    };

    let pg_bind = cfg.pg.bind.clone();
    let pg_task = tokio::spawn(async move {
        server::start_pg_server(&pg_bind).await;
    });

    let snap_dir = cfg.snapshot.dir.clone();
    let snap_prefix = cfg.snapshot.prefix.clone();
    let interval = cfg.snapshot.interval_secs;
    let retain = cfg.snapshot.retain_hours;
    let snapshot_task = tokio::spawn(async move {
        let mut ticker = time::interval(Duration::from_secs(interval));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let t0 = Instant::now();
            match db::save_snapshot(&snap_dir, &snap_prefix).await {
                Ok(path) => info!("Snapshot saved to {} ({:.2?})", path, t0.elapsed()),
                Err(e) => error!("Snapshot failed ({:.2?}): {}", t0.elapsed(), e),
            }
            cleanup_old_snapshots(&snap_dir, &snap_prefix, retain);
        }
    });

    if cfg.web.enabled {
        let app = server::web_router(cfg.snapshot.dir.clone(), cfg.snapshot.prefix.clone());
        let shutdown_snapshot_dir = cfg.snapshot.dir.clone();
        let shutdown_snapshot_prefix = cfg.snapshot.prefix.clone();
        let listener = TokioTcpListener::bind(&cfg.web.bind)
            .await
            .expect("bind web server failed");
        info!("Web console on http://{}", cfg.web.bind);
        axum::serve(listener, app)
            .with_graceful_shutdown(wait_for_shutdown(
                shutdown_snapshot_dir,
                shutdown_snapshot_prefix,
            ))
            .await
            .expect("web server error");
    } else {
        info!("Web console disabled by config");
        wait_for_shutdown(cfg.snapshot.dir.clone(), cfg.snapshot.prefix.clone()).await;
    }

    snapshot_task.abort();
    if let Some(task) = partition_task {
        task.abort();
    }
    pg_task.abort();
    db::shutdown_workers();
}

async fn wait_for_shutdown(snapshot_dir: String, snapshot_prefix: String) {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("Listen shutdown signal failed: {}", e);
        return;
    }

    info!("Shutdown signal received, saving snapshot before exit");
    let t0 = Instant::now();
    match db::save_snapshot(&snapshot_dir, &snapshot_prefix).await {
        Ok(path) => info!("Shutdown snapshot saved to {} ({:.2?})", path, t0.elapsed()),
        Err(e) => error!("Shutdown snapshot failed ({:.2?}): {}", t0.elapsed(), e),
    }
}

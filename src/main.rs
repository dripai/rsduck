use std::path::Path;
use std::time::{Duration, Instant};

use rsduck::{config, db, logging, process_lock, server};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::time;
use tracing::{error, info};

const PROCESS_LOCK_FILE: &str = ".rsduck.lock";
const DEFAULT_RESET_ADMIN_PASSWORD_VALUE: &str = "admin";

fn process_lock_payload(cfg: &config::RsduckConfig, mode: &str) -> serde_json::Value {
    serde_json::json!({
        "pid": std::process::id(),
        "mode": mode,
        "started_at": chrono::Local::now().to_rfc3339(),
        "workdir": std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        "pg_bind": cfg.pg.bind.as_str(),
        "web_bind": cfg.web.bind.as_str(),
    })
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
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let cfg = config::load_config();
    let _log_guard = logging::init_tracing(&cfg.log);

    if let Some(command) = args.first() {
        if command != "reset-admin-password" {
            eprintln!("usage: rsduck reset-admin-password [--password <password>]");
            std::process::exit(2);
        }
        let password = match parse_reset_admin_password_args(&args[1..]) {
            Ok(password) => password,
            Err(e) => {
                eprintln!("{e}");
                eprintln!("usage: rsduck reset-admin-password [--password <password>]");
                std::process::exit(2);
            }
        };
        run_reset_admin_password(&cfg, &password);
        return;
    }

    info!("Config loaded");
    db::validate_snapshot_prefix(&cfg.snapshot.prefix)
        .unwrap_or_else(|e| panic!("invalid snapshot prefix: {e}"));
    let _process_lock = process_lock::ProcessLock::acquire(
        PROCESS_LOCK_FILE,
        process_lock_payload(&cfg, "service"),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    let snapshot_dir = if cfg.snapshot.restore_on_startup {
        db::find_latest_snapshot_dir(&cfg.snapshot.dir, &cfg.snapshot.prefix)
    } else {
        None
    };
    let db = db::DbHandle::open(snapshot_dir.as_deref(), &cfg.db);
    info!("In-memory DuckDB initialized");

    let partition_task = if cfg.partition.maintenance_enabled {
        let interval = cfg.partition.maintenance_interval_secs.max(1);
        let _verify_interval = cfg.partition.verify_interval_secs.max(1);
        let _max_jobs_per_tick = cfg.partition.max_jobs_per_tick.max(1);
        let partition_db = db.clone();
        Some(tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(interval));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let t0 = Instant::now();
                match partition_db.run_partition_maintenance().await {
                    Ok(_) => info!("Partition maintenance completed ({:.2?})", t0.elapsed()),
                    Err(e) => error!("Partition maintenance failed ({:.2?}): {}", t0.elapsed(), e),
                }
            }
        }))
    } else {
        None
    };

    let pg_bind = cfg.pg.bind.clone();
    let pg_db = db.clone();
    let pg_task = tokio::spawn(async move {
        server::start_pg_server(&pg_bind, pg_db).await;
    });

    let snap_dir = cfg.snapshot.dir.clone();
    let snap_prefix = cfg.snapshot.prefix.clone();
    let interval = cfg.snapshot.interval_secs;
    let retain = cfg.snapshot.retain_hours;
    let snapshot_db = db.clone();
    let snapshot_task = tokio::spawn(async move {
        let mut ticker = time::interval(Duration::from_secs(interval));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let t0 = Instant::now();
            match snapshot_db.save_snapshot(&snap_dir, &snap_prefix).await {
                Ok(path) => info!("Snapshot saved to {} ({:.2?})", path, t0.elapsed()),
                Err(e) => error!("Snapshot failed ({:.2?}): {}", t0.elapsed(), e),
            }
            cleanup_old_snapshots(&snap_dir, &snap_prefix, retain);
        }
    });

    if cfg.web.enabled {
        let app = server::web_router(
            db.clone(),
            cfg.snapshot.dir.clone(),
            cfg.snapshot.prefix.clone(),
        );
        let shutdown_snapshot_dir = cfg.snapshot.dir.clone();
        let shutdown_snapshot_prefix = cfg.snapshot.prefix.clone();
        let listener = TokioTcpListener::bind(&cfg.web.bind)
            .await
            .expect("bind web server failed");
        info!("Web console on http://{}", cfg.web.bind);
        axum::serve(listener, app)
            .with_graceful_shutdown(wait_for_shutdown(
                db.clone(),
                shutdown_snapshot_dir,
                shutdown_snapshot_prefix,
            ))
            .await
            .expect("web server error");
    } else {
        info!("Web console disabled by config");
        wait_for_shutdown(
            db.clone(),
            cfg.snapshot.dir.clone(),
            cfg.snapshot.prefix.clone(),
        )
        .await;
    }

    snapshot_task.abort();
    if let Some(task) = partition_task {
        task.abort();
    }
    pg_task.abort();
    db.shutdown();
}

fn parse_reset_admin_password_args(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(DEFAULT_RESET_ADMIN_PASSWORD_VALUE.to_string()),
        [flag, password] if flag == "--password" => {
            if password.is_empty() {
                Err("reset admin password cannot be empty".into())
            } else {
                Ok(password.clone())
            }
        }
        _ => Err("invalid reset-admin-password arguments".into()),
    }
}

fn run_reset_admin_password(cfg: &config::RsduckConfig, password: &str) {
    if let Err(e) = db::validate_snapshot_prefix(&cfg.snapshot.prefix) {
        eprintln!("invalid snapshot prefix: {e}");
        std::process::exit(1);
    }
    let _process_lock = match process_lock::ProcessLock::acquire(
        PROCESS_LOCK_FILE,
        process_lock_payload(cfg, "reset-admin-password"),
    ) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    match db::reset_admin_password_offline(&cfg.snapshot.dir, &cfg.snapshot.prefix, password) {
        Ok(path) => {
            println!("admin password reset; new snapshot: {}", path);
        }
        Err(e) => {
            eprintln!("reset admin password failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod main_tests {
    use super::{parse_reset_admin_password_args, DEFAULT_RESET_ADMIN_PASSWORD_VALUE};

    #[test]
    fn reset_admin_password_args_default_to_admin() {
        let args: Vec<String> = vec![];
        assert_eq!(
            parse_reset_admin_password_args(&args).unwrap(),
            DEFAULT_RESET_ADMIN_PASSWORD_VALUE
        );
    }

    #[test]
    fn reset_admin_password_args_accept_password_flag() {
        let args = vec!["--password".to_string(), "admin123".to_string()];
        assert_eq!(parse_reset_admin_password_args(&args).unwrap(), "admin123");
    }

    #[test]
    fn reset_admin_password_args_reject_unknown_shape() {
        let args = vec!["--password".to_string()];
        assert!(parse_reset_admin_password_args(&args).is_err());
    }
}

async fn wait_for_shutdown(db: db::DbHandle, snapshot_dir: String, snapshot_prefix: String) {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("Listen shutdown signal failed: {}", e);
        return;
    }

    info!("Shutdown signal received, saving snapshot before exit");
    let t0 = Instant::now();
    match db.save_snapshot(&snapshot_dir, &snapshot_prefix).await {
        Ok(path) => info!("Shutdown snapshot saved to {} ({:.2?})", path, t0.elapsed()),
        Err(e) => error!("Shutdown snapshot failed ({:.2?}): {}", t0.elapsed(), e),
    }
}

use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct RsduckConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
    #[serde(default)]
    pub partition: PartitionConfig,
    #[serde(default)]
    pub pg: PgConfig,
    #[serde(default)]
    pub web: WebConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    #[serde(default = "default_init_sql")]
    pub init_sql: String,
    #[serde(default = "default_read_workers")]
    pub read_workers: usize,
    #[serde(default = "default_write_queue_size")]
    pub write_queue_size: usize,
    #[serde(default = "default_read_queue_size")]
    pub read_queue_size: usize,
    #[serde(default = "default_snapshot_queue_size")]
    pub snapshot_queue_size: usize,
    #[serde(default = "default_max_result_rows")]
    pub max_result_rows: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotConfig {
    #[serde(default = "default_true")]
    pub restore_on_startup: bool,
    #[serde(default = "default_snapshot_dir")]
    pub dir: String,
    #[serde(default = "default_snapshot_prefix")]
    pub prefix: String,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_retain_hours")]
    pub retain_hours: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PartitionConfig {
    #[serde(default = "default_true")]
    pub maintenance_enabled: bool,
    #[serde(default = "default_partition_maintenance_interval_secs")]
    pub maintenance_interval_secs: u64,
    #[serde(default = "default_partition_verify_interval_secs")]
    pub verify_interval_secs: u64,
    #[serde(default = "default_partition_max_jobs_per_tick")]
    pub max_jobs_per_tick: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PgConfig {
    #[serde(default = "default_pg_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_web_bind")]
    pub bind: String,
}

fn default_true() -> bool {
    true
}

fn default_snapshot_dir() -> String {
    "snapshot".into()
}

fn default_snapshot_prefix() -> String {
    "rsduck".into()
}

fn default_init_sql() -> String {
    String::new()
}

fn default_read_workers() -> usize {
    4
}

fn default_write_queue_size() -> usize {
    100_000
}

fn default_read_queue_size() -> usize {
    1024
}

fn default_snapshot_queue_size() -> usize {
    16
}

fn default_max_result_rows() -> usize {
    100_000
}

fn default_interval_secs() -> u64 {
    900
}

fn default_retain_hours() -> u64 {
    2
}

fn default_partition_maintenance_interval_secs() -> u64 {
    60
}

fn default_partition_verify_interval_secs() -> u64 {
    300
}

fn default_partition_max_jobs_per_tick() -> usize {
    100
}

fn default_pg_bind() -> String {
    "127.0.0.1:15432".into()
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".into()
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            restore_on_startup: default_true(),
            dir: default_snapshot_dir(),
            prefix: default_snapshot_prefix(),
            interval_secs: default_interval_secs(),
            retain_hours: default_retain_hours(),
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            init_sql: default_init_sql(),
            read_workers: default_read_workers(),
            write_queue_size: default_write_queue_size(),
            read_queue_size: default_read_queue_size(),
            snapshot_queue_size: default_snapshot_queue_size(),
            max_result_rows: default_max_result_rows(),
        }
    }
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            maintenance_enabled: default_true(),
            maintenance_interval_secs: default_partition_maintenance_interval_secs(),
            verify_interval_secs: default_partition_verify_interval_secs(),
            max_jobs_per_tick: default_partition_max_jobs_per_tick(),
        }
    }
}

impl Default for PgConfig {
    fn default() -> Self {
        Self {
            bind: default_pg_bind(),
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            bind: default_web_bind(),
        }
    }
}

impl Default for RsduckConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            db: DbConfig::default(),
            snapshot: SnapshotConfig::default(),
            partition: PartitionConfig::default(),
            pg: PgConfig::default(),
            web: WebConfig::default(),
        }
    }
}

pub fn load_config() -> RsduckConfig {
    let path = Path::new("rsduck.toml");
    if path.exists() {
        let content = fs::read_to_string(path).expect("failed to read rsduck.toml");
        toml::from_str(&content).expect("failed to parse rsduck.toml")
    } else {
        tracing::info!("rsduck.toml not found, using defaults");
        RsduckConfig::default()
    }
}

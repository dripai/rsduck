use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RsduckConfig {
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
    #[serde(default)]
    pub partition: PartitionConfig,
    #[serde(default)]
    pub mysql: MysqlConfig,
    #[serde(default)]
    pub web: WebConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_dir")]
    pub dir: String,
    #[serde(default = "default_log_file_prefix")]
    pub file_prefix: String,
    #[serde(default = "default_log_retain_files")]
    pub retain_files: usize,
    #[serde(default)]
    pub console: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default = "default_extension_dir")]
    pub extension_dir: String,
    #[serde(default = "default_true")]
    pub vss_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct MysqlConfig {
    #[serde(default = "default_mysql_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_web_bind")]
    pub bind: String,
    #[serde(default = "default_parquet_import_root")]
    pub parquet_import_root: String,
    #[serde(default)]
    pub vector_api_tokens: Vec<VectorApiTokenConfig>,
    #[serde(default)]
    pub vector_api_limits: VectorApiLimitsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorApiLimitsConfig {
    #[serde(default = "default_vector_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_vector_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_vector_search_timeout_ms")]
    pub search_timeout_ms: u64,
    #[serde(default = "default_vector_write_timeout_ms")]
    pub write_timeout_ms: u64,
    #[serde(default = "default_vector_maintenance_timeout_ms")]
    pub maintenance_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorApiTokenConfig {
    pub token: String,
    pub username: String,
    pub tenant_ids: Vec<i64>,
    #[serde(default)]
    pub agent_ids: Vec<i64>,
    pub vector_spaces: Vec<String>,
    pub permissions: Vec<String>,
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

fn default_parquet_import_root() -> String {
    ".".into()
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

fn default_extension_dir() -> String {
    "extensions".into()
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

fn default_mysql_bind() -> String {
    "127.0.0.1:13306".into()
}

fn default_web_bind() -> String {
    "127.0.0.1:13307".into()
}

fn default_vector_max_body_bytes() -> usize {
    32 * 1024 * 1024
}

fn default_vector_max_concurrent_requests() -> usize {
    64
}

fn default_vector_search_timeout_ms() -> u64 {
    5_000
}

fn default_vector_write_timeout_ms() -> u64 {
    30_000
}

fn default_vector_maintenance_timeout_ms() -> u64 {
    300_000
}

fn default_log_level() -> String {
    "info".into()
}

fn default_log_dir() -> String {
    "logs".into()
}

fn default_log_file_prefix() -> String {
    "rsduck".into()
}

fn default_log_retain_files() -> usize {
    3
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            dir: default_log_dir(),
            file_prefix: default_log_file_prefix(),
            retain_files: default_log_retain_files(),
            console: false,
        }
    }
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
            extension_dir: default_extension_dir(),
            vss_enabled: true,
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

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            bind: default_web_bind(),
            parquet_import_root: default_parquet_import_root(),
            vector_api_tokens: Vec::new(),
            vector_api_limits: VectorApiLimitsConfig::default(),
        }
    }
}

impl Default for VectorApiLimitsConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: default_vector_max_body_bytes(),
            max_concurrent_requests: default_vector_max_concurrent_requests(),
            search_timeout_ms: default_vector_search_timeout_ms(),
            write_timeout_ms: default_vector_write_timeout_ms(),
            maintenance_timeout_ms: default_vector_maintenance_timeout_ms(),
        }
    }
}

impl Default for MysqlConfig {
    fn default() -> Self {
        Self {
            bind: default_mysql_bind(),
        }
    }
}

impl Default for RsduckConfig {
    fn default() -> Self {
        Self {
            log: LogConfig::default(),
            db: DbConfig::default(),
            snapshot: SnapshotConfig::default(),
            partition: PartitionConfig::default(),
            mysql: MysqlConfig::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_config_parses_vector_api_limits() {
        let config: RsduckConfig = toml::from_str(include_str!("../rsduck.toml")).unwrap();
        assert_eq!(config.web.vector_api_limits.max_body_bytes, 33_554_432);
        assert_eq!(config.web.vector_api_limits.max_concurrent_requests, 64);
        assert_eq!(config.web.vector_api_limits.search_timeout_ms, 5_000);
        assert_eq!(config.web.vector_api_limits.write_timeout_ms, 30_000);
        assert_eq!(config.web.vector_api_limits.maintenance_timeout_ms, 300_000);
    }

    #[test]
    fn vector_api_limits_have_safe_defaults_when_section_is_absent() {
        let config: RsduckConfig = toml::from_str("").unwrap();
        assert_eq!(config.web.vector_api_limits.max_concurrent_requests, 64);
        assert_eq!(
            config.web.vector_api_limits.max_body_bytes,
            32 * 1024 * 1024
        );
    }
}

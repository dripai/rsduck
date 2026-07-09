use crate::config::LogConfig;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub fn init_tracing(log: &LogConfig) -> WorkerGuard {
    validate_log_config(log);
    fs::create_dir_all(&log.dir)
        .unwrap_or_else(|e| panic!("failed to create log directory {}: {e}", log.dir));
    cleanup_old_log_files(log);

    let appender = tracing_appender::rolling::daily(&log.dir, &log.file_name);
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let level = parse_log_level(&log.level);
    let timer = tracing_subscriber::fmt::time::LocalTime::rfc_3339();

    if log.console {
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .with_timer(timer)
            .with_writer(file_writer.and(std::io::stdout))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .with_timer(timer)
            .with_writer(file_writer)
            .init();
    }

    guard
}

fn parse_log_level(level: &str) -> LevelFilter {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" | "warning" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "off" => LevelFilter::OFF,
        _ => panic!(
            "invalid log.level in rsduck.toml: {level}; expected trace, debug, info, warn, error, or off"
        ),
    }
}

fn validate_log_config(log: &LogConfig) {
    if log.dir.trim().is_empty() {
        panic!("invalid log.dir in rsduck.toml: value cannot be empty");
    }
    if log.file_name.trim().is_empty()
        || log.file_name.contains('/')
        || log.file_name.contains('\\')
    {
        panic!("invalid log.file_name in rsduck.toml: use a file name without path separators");
    }
    if log.retain_files == 0 {
        panic!("invalid log.retain_files in rsduck.toml: value must be greater than 0");
    }
}

fn cleanup_old_log_files(log: &LogConfig) {
    let entries = fs::read_dir(&log.dir)
        .unwrap_or_else(|e| panic!("failed to read log directory {}: {e}", log.dir));
    let current_daily_name = format!(
        "{}.{}",
        log.file_name,
        chrono::Local::now().format("%Y-%m-%d")
    );
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_managed_log_file(file_name, &log.file_name) {
            continue;
        }
        let file_name = file_name.to_string();

        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push(ManagedLogFile {
            path,
            file_name,
            modified,
        });
    }

    let current_file_exists = files
        .iter()
        .any(|file| file.file_name == current_daily_name);
    let keep_count = if current_file_exists {
        log.retain_files
    } else {
        log.retain_files.saturating_sub(1)
    };

    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    for file in files.into_iter().skip(keep_count) {
        let _ = fs::remove_file(file.path);
    }
}

fn is_managed_log_file(file_name: &str, configured_file_name: &str) -> bool {
    file_name == configured_file_name || file_name.starts_with(&format!("{configured_file_name}."))
}

struct ManagedLogFile {
    path: PathBuf,
    file_name: String,
    modified: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::{is_managed_log_file, parse_log_level};
    use tracing_subscriber::filter::LevelFilter;

    #[test]
    fn log_level_parser_accepts_supported_values() {
        assert_eq!(parse_log_level("trace"), LevelFilter::TRACE);
        assert_eq!(parse_log_level("DEBUG"), LevelFilter::DEBUG);
        assert_eq!(parse_log_level("info"), LevelFilter::INFO);
        assert_eq!(parse_log_level("warning"), LevelFilter::WARN);
        assert_eq!(parse_log_level("error"), LevelFilter::ERROR);
        assert_eq!(parse_log_level("off"), LevelFilter::OFF);
    }

    #[test]
    fn managed_log_file_matches_current_and_rotated_names() {
        assert!(is_managed_log_file("rsduck.log", "rsduck.log"));
        assert!(is_managed_log_file("rsduck.log.2026-07-09", "rsduck.log"));
        assert!(!is_managed_log_file("rsduck-service.out.log", "rsduck.log"));
    }
}

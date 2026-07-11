use crate::config::LogConfig;
use chrono::{Local, NaiveDate};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing(log: &LogConfig) -> WorkerGuard {
    validate_log_config(log);
    fs::create_dir_all(&log.dir)
        .unwrap_or_else(|e| panic!("failed to create log directory {}: {e}", log.dir));

    let appender = LocalDailyLogWriter::new(&log.dir, &log.file_prefix, log.retain_files)
        .unwrap_or_else(|e| panic!("failed to initialize log file writer: {e}"));
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let level = parse_log_level(&log.level);
    let timer = tracing_subscriber::fmt::time::LocalTime::rfc_3339();

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_timer(timer.clone())
        .with_writer(file_writer);

    if log.console {
        let console_layer = fmt::layer()
            .with_ansi(true)
            .with_target(false)
            .with_timer(timer)
            .with_writer(std::io::stdout);

        tracing_subscriber::registry()
            .with(level)
            .with(file_layer)
            .with(console_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(level)
            .with(file_layer)
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
    if log.file_prefix.trim().is_empty()
        || log.file_prefix.contains('/')
        || log.file_prefix.contains('\\')
        || log.file_prefix.contains('.')
    {
        panic!("invalid log.file_prefix in rsduck.toml: use a plain prefix without path separators or dots");
    }
    if log.retain_files == 0 {
        panic!("invalid log.retain_files in rsduck.toml: value must be greater than 0");
    }
}

fn cleanup_old_log_files(dir: &Path, file_prefix: &str, retain_files: usize) -> io::Result<()> {
    let entries = fs::read_dir(dir)?;
    let current_daily_name = current_log_file_name(file_prefix);
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(date) = managed_log_file_date(file_name, file_prefix) else {
            continue;
        };
        let file_name = file_name.to_string();
        files.push(ManagedLogFile {
            path,
            file_name,
            date,
        });
    }

    let current_file_exists = files
        .iter()
        .any(|file| file.file_name == current_daily_name);
    let keep_count = if current_file_exists {
        retain_files
    } else {
        retain_files.saturating_sub(1)
    };

    files.sort_by(|a, b| b.date.cmp(&a.date));
    for file in files.into_iter().skip(keep_count) {
        let _ = fs::remove_file(file.path);
    }

    Ok(())
}

fn current_log_file_name(file_prefix: &str) -> String {
    log_file_name_for_date(file_prefix, &Local::now().format("%Y-%m-%d").to_string())
}

fn log_file_name_for_date(file_prefix: &str, date: &str) -> String {
    format!("{file_prefix}.{date}.log")
}

fn managed_log_file_date(file_name: &str, file_prefix: &str) -> Option<NaiveDate> {
    let date = file_name
        .strip_prefix(&format!("{file_prefix}."))
        .and_then(|name| name.strip_suffix(".log"))?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn open_log_file(dir: &Path, file_prefix: &str, date: &str) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(log_file_name_for_date(file_prefix, date)))
}

struct LocalDailyLogWriter {
    dir: PathBuf,
    file_prefix: String,
    retain_files: usize,
    current_date: String,
    file: File,
}

impl LocalDailyLogWriter {
    fn new(dir: impl Into<PathBuf>, file_prefix: &str, retain_files: usize) -> io::Result<Self> {
        let dir = dir.into();
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        let file = open_log_file(&dir, file_prefix, &current_date)?;
        cleanup_old_log_files(&dir, file_prefix, retain_files)?;

        Ok(Self {
            dir,
            file_prefix: file_prefix.to_string(),
            retain_files,
            current_date,
            file,
        })
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        if self.current_date == current_date {
            return Ok(());
        }

        self.file.flush()?;
        self.file = open_log_file(&self.dir, &self.file_prefix, &current_date)?;
        self.current_date = current_date;
        cleanup_old_log_files(&self.dir, &self.file_prefix, self.retain_files)
    }
}

impl Write for LocalDailyLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed()?;
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.rotate_if_needed()?;
        self.file.flush()
    }
}

struct ManagedLogFile {
    path: PathBuf,
    file_name: String,
    date: NaiveDate,
}

#[cfg(test)]
mod tests {
    use super::{log_file_name_for_date, managed_log_file_date, parse_log_level};
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
    fn log_file_name_uses_prefix_date_and_log_suffix() {
        assert_eq!(
            log_file_name_for_date("rsduck", "2026-07-10"),
            "rsduck.2026-07-10.log"
        );
    }

    #[test]
    fn managed_log_file_matches_only_new_daily_names() {
        assert!(managed_log_file_date("rsduck.2026-07-09.log", "rsduck").is_some());
        assert!(managed_log_file_date("rsduck.log.2026-07-09", "rsduck").is_none());
        assert!(managed_log_file_date("rsduck.log", "rsduck").is_none());
        assert!(managed_log_file_date("rsduck-service.2026-07-09.log", "rsduck").is_none());
    }
}

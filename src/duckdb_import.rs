use chrono::Local;
use duckdb::{AccessMode, Config, Connection};
use reqwest::blocking::Client;
use reqwest::header::{COOKIE, SET_COOKIE};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:13307";
const DEFAULT_USERNAME: &str = "admin";
const IMPORT_REQUEST_TIMEOUT_SECS: u64 = 30 * 60;
const TEMP_DIR_NAME: &str = ".rsduck-import";

#[derive(Clone)]
pub struct ImportDuckDbConfig {
    pub source: PathBuf,
    pub target_schema: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub tables: Vec<String>,
    pub dry_run: bool,
    pub keep_temp: bool,
    pub report: Option<PathBuf>,
    pub if_exists: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportDuckDbReport {
    pub migration_id: String,
    pub source: String,
    pub target_schema: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub tables: Vec<ImportTableReport>,
}

impl ImportDuckDbReport {
    pub fn exit_code(&self) -> i32 {
        if self.failed == 0 {
            0
        } else {
            2
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportTableReport {
    pub source_schema: String,
    pub source_table: String,
    pub target_table: String,
    pub status: String,
    pub source_rows: Option<u64>,
    pub target_rows: Option<u64>,
    pub elapsed_ms: u128,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct SourceTable {
    schema: String,
    table: String,
    preflight_error: Option<String>,
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct BasicResponse {
    success: bool,
    msg: String,
}

#[derive(Deserialize)]
struct ImportInfoResponse {
    success: bool,
    root: String,
    msg: String,
}

#[derive(Serialize)]
struct SqlRequest<'a> {
    sql: &'a str,
    page: usize,
    page_size: usize,
}

#[derive(Serialize)]
struct ImportRequest<'a> {
    source: &'a str,
    schema: &'a str,
    table: Option<&'a str>,
}

#[derive(Deserialize)]
struct ImportResponse {
    success: bool,
    msg: String,
    rows: usize,
}

enum ImportCallError {
    Rejected(String),
    Unknown(String),
}

struct RsduckWebClient {
    client: Client,
    endpoint: String,
    cookie: String,
}

impl RsduckWebClient {
    fn login(endpoint: &str, username: &str, password: &str) -> Result<Self, String> {
        let endpoint = endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return Err("import-duckdb endpoint cannot be empty".into());
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(IMPORT_REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|error| format!("create RSDuck HTTP client failed: {error}"))?;
        let response = client
            .post(format!("{endpoint}/login"))
            .json(&LoginRequest { username, password })
            .send()
            .map_err(|error| format!("connect to RSDuck login endpoint failed: {error}"))?;
        let cookie = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(session_cookie);
        let body: BasicResponse = response
            .json()
            .map_err(|error| format!("decode RSDuck login response failed: {error}"))?;
        if !body.success {
            return Err(format!("RSDuck login failed: {}", body.msg));
        }
        let cookie =
            cookie.ok_or_else(|| "RSDuck login did not return a session cookie".to_string())?;
        Ok(Self {
            client,
            endpoint,
            cookie,
        })
    }

    fn import_root(&self) -> Result<PathBuf, String> {
        let response = self
            .client
            .get(format!("{}/parquet-import", self.endpoint))
            .header(COOKIE, &self.cookie)
            .send()
            .map_err(|error| format!("query RSDuck Parquet import root failed: {error}"))?;
        let body: ImportInfoResponse = response
            .json()
            .map_err(|error| format!("decode RSDuck Parquet import root failed: {error}"))?;
        if !body.success {
            return Err(format!(
                "query RSDuck Parquet import root failed: {}",
                body.msg
            ));
        }
        let root = PathBuf::from(body.root);
        if !root.is_absolute() {
            return Err(format!(
                "RSDuck Parquet import root is not absolute: {}",
                root.display()
            ));
        }
        Ok(root)
    }

    fn execute_sql(&self, sql: &str) -> Result<(), String> {
        let response = self
            .client
            .post(format!("{}/sql", self.endpoint))
            .header(COOKIE, &self.cookie)
            .json(&SqlRequest {
                sql,
                page: 0,
                page_size: 1,
            })
            .send()
            .map_err(|error| format!("execute RSDuck SQL failed: {error}"))?;
        let body: BasicResponse = response
            .json()
            .map_err(|error| format!("decode RSDuck SQL response failed: {error}"))?;
        if body.success {
            Ok(())
        } else {
            Err(body.msg)
        }
    }

    fn import_table(
        &self,
        source: &str,
        schema: &str,
        table: &str,
    ) -> Result<usize, ImportCallError> {
        let response = self
            .client
            .post(format!("{}/parquet-import", self.endpoint))
            .header(COOKIE, &self.cookie)
            .json(&ImportRequest {
                source,
                schema,
                table: Some(table),
            })
            .send()
            .map_err(|error| {
                ImportCallError::Unknown(format!(
                    "submit RSDuck table import failed; target state is unknown: {error}"
                ))
            })?;
        let body: ImportResponse = response.json().map_err(|error| {
            ImportCallError::Unknown(format!(
                "decode RSDuck table import response failed; target state is unknown: {error}"
            ))
        })?;
        if body.success {
            Ok(body.rows)
        } else {
            Err(ImportCallError::Rejected(body.msg))
        }
    }

    fn logout(&self) {
        let _ = self
            .client
            .post(format!("{}/logout", self.endpoint))
            .header(COOKIE, &self.cookie)
            .timeout(Duration::from_secs(5))
            .send();
    }
}

impl Drop for RsduckWebClient {
    fn drop(&mut self) {
        self.logout();
    }
}

pub fn parse_import_duckdb_args(args: &[String]) -> Result<ImportDuckDbConfig, String> {
    let mut source = None;
    let mut target_schema = None;
    let mut endpoint = None;
    let mut username = None;
    let mut password = None;
    let mut tables = None;
    let mut report = None;
    let mut if_exists = None;
    let mut dry_run = false;
    let mut keep_temp = false;
    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--dry-run" => {
                if dry_run {
                    return Err("duplicate import-duckdb option: --dry-run".into());
                }
                dry_run = true;
                index += 1;
            }
            "--keep-temp" => {
                if keep_temp {
                    return Err("duplicate import-duckdb option: --keep-temp".into());
                }
                keep_temp = true;
                index += 1;
            }
            "--source" | "--target-schema" | "--endpoint" | "--username" | "--password"
            | "--tables" | "--report" | "--if-exists" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for import-duckdb option: {flag}"))?;
                if value.trim().is_empty() {
                    return Err(format!("empty value for import-duckdb option: {flag}"));
                }
                match flag {
                    "--source" => set_once(&mut source, PathBuf::from(value), flag)?,
                    "--target-schema" => {
                        set_once(&mut target_schema, value.trim().to_string(), flag)?
                    }
                    "--endpoint" => set_once(&mut endpoint, value.trim().to_string(), flag)?,
                    "--username" => set_once(&mut username, value.trim().to_string(), flag)?,
                    "--password" => set_once(&mut password, value.to_string(), flag)?,
                    "--tables" => {
                        let selected = value
                            .split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        if selected.is_empty() {
                            return Err("--tables must include at least one table".into());
                        }
                        set_once(&mut tables, selected, flag)?;
                    }
                    "--report" => set_once(&mut report, PathBuf::from(value), flag)?,
                    "--if-exists" => {
                        set_once(&mut if_exists, value.trim().to_ascii_lowercase(), flag)?
                    }
                    _ => unreachable!(),
                }
                index += 2;
            }
            _ => return Err(format!("unknown import-duckdb option: {flag}")),
        }
    }

    let target_schema = target_schema.ok_or("missing required option: --target-schema")?;
    validate_target_identifier("schema", &target_schema)?;
    let password = password.ok_or("missing required option: --password")?;
    let if_exists = if_exists.unwrap_or_else(|| "error".to_string());
    if if_exists != "error" {
        return Err("--if-exists currently supports only: error".into());
    }

    Ok(ImportDuckDbConfig {
        source: source.ok_or("missing required option: --source")?,
        target_schema,
        endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        username: username.unwrap_or_else(|| DEFAULT_USERNAME.to_string()),
        password,
        tables: tables.unwrap_or_default(),
        dry_run,
        keep_temp,
        report,
        if_exists,
    })
}

pub fn run_import_duckdb(config: ImportDuckDbConfig) -> Result<ImportDuckDbReport, String> {
    validate_config(&config)?;
    let started_at = Local::now();
    let migration_id = new_migration_id();
    let source_path = fs::canonicalize(&config.source).map_err(|error| {
        format!(
            "resolve import-duckdb source failed: {}: {error}",
            config.source.display()
        )
    })?;
    if !source_path.is_file() {
        return Err(format!(
            "import-duckdb source must be a file: {}",
            source_path.display()
        ));
    }
    let source_display = duckdb_path(&source_path);

    let source_config = Config::default()
        .access_mode(AccessMode::ReadOnly)
        .map_err(|error| format!("configure source DuckDB read-only access failed: {error}"))?;
    let source = Connection::open_with_flags(&source_path, source_config)
        .map_err(|error| format!("open source DuckDB read-only failed: {error}"))?;
    source
        .execute_batch("BEGIN TRANSACTION")
        .map_err(|error| format!("begin source DuckDB read transaction failed: {error}"))?;

    let mut source_tables = discover_source_tables(&source)?;
    if source_tables.is_empty() {
        return Err("source DuckDB contains no ordinary persistent tables".into());
    }
    apply_table_filter(&mut source_tables, &config.tables)?;
    mark_duplicate_targets(&mut source_tables);
    mark_unsupported_tables(&source, &mut source_tables)?;

    let web = RsduckWebClient::login(&config.endpoint, &config.username, &config.password)?;

    println!("DuckDB import plan");
    println!("Source:        {source_display}");
    println!("Target schema: {}", config.target_schema);
    println!("Tables:        {}", source_tables.len());

    if config.dry_run {
        let mut table_reports = Vec::with_capacity(source_tables.len());
        for table in source_tables {
            let failed = table.preflight_error.is_some();
            table_reports.push(ImportTableReport {
                source_schema: table.schema,
                source_table: table.table.clone(),
                target_table: table.table,
                status: if failed { "failed" } else { "planned" }.to_string(),
                source_rows: None,
                target_rows: None,
                elapsed_ms: 0,
                error: table.preflight_error,
            });
        }
        let failed = table_reports
            .iter()
            .filter(|table| table.status == "failed")
            .count();
        let report = ImportDuckDbReport {
            migration_id,
            source: source_display.clone(),
            target_schema: config.target_schema.clone(),
            started_at: started_at.to_rfc3339(),
            finished_at: Local::now().to_rfc3339(),
            status: if failed == 0 {
                "dry_run".into()
            } else {
                "dry_run_failed".into()
            },
            total: table_reports.len(),
            succeeded: 0,
            failed,
            tables: table_reports,
        };
        write_report(&config, &report)?;
        let _ = source.execute_batch("ROLLBACK");
        print_summary(&report);
        return Ok(report);
    }

    let import_root = web.import_root()?;

    web.execute_sql(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&config.target_schema)
    ))
    .map_err(|error| format!("create target schema failed: {error}"))?;

    let relative_job_dir = PathBuf::from(TEMP_DIR_NAME).join(&migration_id);
    let job_dir = import_root.join(&relative_job_dir);
    fs::create_dir_all(&job_dir).map_err(|error| {
        format!(
            "create import-duckdb temporary directory failed: {}: {error}",
            job_dir.display()
        )
    })?;

    let total = source_tables.len();
    let mut table_reports = Vec::with_capacity(total);
    for (position, table) in source_tables.into_iter().enumerate() {
        let table_started = Instant::now();
        println!(
            "[{}/{}] {}.{} -> {}.{}",
            position + 1,
            total,
            table.schema,
            table.table,
            config.target_schema,
            table.table
        );
        if let Some(error) = table.preflight_error.clone() {
            println!("  FAILED: {error}");
            table_reports.push(failed_table_report(
                &table,
                None,
                table_started.elapsed().as_millis(),
                error,
            ));
            continue;
        }

        let source_rows = match source_row_count(&source, &table) {
            Ok(rows) => rows,
            Err(error) => {
                println!("  FAILED: {error}");
                table_reports.push(failed_table_report(
                    &table,
                    None,
                    table_started.elapsed().as_millis(),
                    error,
                ));
                continue;
            }
        };
        let file_name = format!("{:06}.parquet", position + 1);
        let parquet_path = job_dir.join(&file_name);
        let relative_parquet = relative_job_dir.join(&file_name);
        let export_result = export_source_table(&source, &table, &parquet_path);
        if let Err(error) = export_result {
            let _ = fs::remove_file(&parquet_path);
            println!("  FAILED: {error}");
            table_reports.push(failed_table_report(
                &table,
                Some(source_rows),
                table_started.elapsed().as_millis(),
                error,
            ));
            continue;
        }

        let source_text = path_for_http(&relative_parquet);
        let import_result = web.import_table(&source_text, &config.target_schema, &table.table);
        if !config.keep_temp {
            let _ = fs::remove_file(&parquet_path);
        }
        match import_result {
            Ok(target_rows) if target_rows as u64 == source_rows => {
                println!("  OK: {target_rows} row(s)");
                table_reports.push(ImportTableReport {
                    source_schema: table.schema,
                    source_table: table.table.clone(),
                    target_table: table.table,
                    status: "succeeded".into(),
                    source_rows: Some(source_rows),
                    target_rows: Some(target_rows as u64),
                    elapsed_ms: table_started.elapsed().as_millis(),
                    error: None,
                });
            }
            Ok(target_rows) => {
                let mismatch =
                    format!("row count mismatch: source={source_rows}, target={target_rows}");
                let cleanup = web.execute_sql(&format!(
                    "DROP TABLE {}.{}",
                    quote_ident(&config.target_schema),
                    quote_ident(&table.table)
                ));
                let error = match cleanup {
                    Ok(()) => mismatch,
                    Err(cleanup_error) => {
                        format!("{mismatch}; cleanup target table failed: {cleanup_error}")
                    }
                };
                println!("  FAILED: {error}");
                table_reports.push(failed_table_report_with_target(
                    &table,
                    source_rows,
                    target_rows as u64,
                    table_started.elapsed().as_millis(),
                    error,
                ));
            }
            Err(ImportCallError::Rejected(error)) => {
                println!("  FAILED: {error}");
                table_reports.push(failed_table_report(
                    &table,
                    Some(source_rows),
                    table_started.elapsed().as_millis(),
                    error,
                ));
            }
            Err(ImportCallError::Unknown(error)) => {
                println!("  UNKNOWN: {error}");
                table_reports.push(unknown_table_report(
                    &table,
                    source_rows,
                    table_started.elapsed().as_millis(),
                    error,
                ));
            }
        }
    }

    if !config.keep_temp {
        let _ = fs::remove_dir(&job_dir);
        if let Some(parent) = job_dir.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    let _ = source.execute_batch("ROLLBACK");

    let succeeded = table_reports
        .iter()
        .filter(|table| table.status == "succeeded")
        .count();
    let failed = table_reports.len() - succeeded;
    let report = ImportDuckDbReport {
        migration_id,
        source: source_display,
        target_schema: config.target_schema.clone(),
        started_at: started_at.to_rfc3339(),
        finished_at: Local::now().to_rfc3339(),
        status: if failed == 0 {
            "succeeded".into()
        } else {
            "partial_failure".into()
        },
        total: table_reports.len(),
        succeeded,
        failed,
        tables: table_reports,
    };
    write_report(&config, &report)?;
    print_summary(&report);
    Ok(report)
}

pub fn import_duckdb_usage() -> &'static str {
    "usage: rsduck import-duckdb --source <file.duckdb> --target-schema <schema> --password <password> [--endpoint <url>] [--username <user>] [--tables <schema.table,table>] [--dry-run] [--keep-temp] [--report <file.json>] [--if-exists error]"
}

fn validate_config(config: &ImportDuckDbConfig) -> Result<(), String> {
    validate_target_identifier("schema", &config.target_schema)?;
    if config.username.trim().is_empty() {
        return Err("import-duckdb username cannot be empty".into());
    }
    if config.password.is_empty() {
        return Err("import-duckdb password cannot be empty".into());
    }
    if config.if_exists != "error" {
        return Err("import-duckdb if-exists currently supports only: error".into());
    }
    Ok(())
}

fn discover_source_tables(conn: &Connection) -> Result<Vec<SourceTable>, String> {
    let mut statement = conn
        .prepare(
            "SELECT schema_name, table_name
             FROM duckdb_tables()
             WHERE database_name = current_database()
               AND NOT internal
               AND NOT temporary
             ORDER BY lower(schema_name), lower(table_name), schema_name, table_name",
        )
        .map_err(|error| format!("prepare source DuckDB table discovery failed: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(SourceTable {
                schema: row.get(0)?,
                table: row.get(1)?,
                preflight_error: None,
            })
        })
        .map_err(|error| format!("query source DuckDB tables failed: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read source DuckDB table metadata failed: {error}"))
}

fn apply_table_filter(tables: &mut Vec<SourceTable>, selectors: &[String]) -> Result<(), String> {
    if selectors.is_empty() {
        return Ok(());
    }
    let mut matched = HashSet::new();
    tables.retain(|table| {
        let qualified = format!("{}.{}", table.schema, table.table);
        let selected = selectors.iter().any(|selector| {
            let is_match = if selector.contains('.') {
                selector.eq_ignore_ascii_case(&qualified)
            } else {
                selector.eq_ignore_ascii_case(&table.table)
            };
            if is_match {
                matched.insert(selector.to_ascii_lowercase());
            }
            is_match
        });
        selected
    });
    let missing = selectors
        .iter()
        .filter(|selector| !matched.contains(&selector.to_ascii_lowercase()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "selected source tables do not exist: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

fn mark_duplicate_targets(tables: &mut [SourceTable]) {
    let mut counts = HashMap::<String, usize>::new();
    for table in tables.iter() {
        *counts.entry(table.table.to_ascii_lowercase()).or_default() += 1;
    }
    for table in tables.iter_mut() {
        if counts
            .get(&table.table.to_ascii_lowercase())
            .copied()
            .unwrap_or_default()
            > 1
        {
            table.preflight_error = Some(format!(
                "duplicate target table name across source schemas: {}",
                table.table
            ));
        }
    }
}

fn mark_unsupported_tables(conn: &Connection, tables: &mut [SourceTable]) -> Result<(), String> {
    for table in tables.iter_mut() {
        if table.preflight_error.is_some() {
            continue;
        }
        if let Err(error) = validate_target_identifier("table", &table.table) {
            table.preflight_error = Some(error);
            continue;
        }
        let mut statement = conn
            .prepare(
                "SELECT data_type
                 FROM duckdb_columns()
                 WHERE database_name = current_database()
                   AND schema_name = ?
                   AND table_name = ?
                 ORDER BY column_index",
            )
            .map_err(|error| format!("prepare source DuckDB type inspection failed: {error}"))?;
        let types = statement
            .query_map([table.schema.as_str(), table.table.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| format!("query source DuckDB column types failed: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read source DuckDB column types failed: {error}"))?;
        if let Some(data_type) = types.iter().find(|data_type| has_fixed_array(data_type)) {
            table.preflight_error = Some(format!(
                "fixed-size array type cannot be safely migrated through Parquet: {data_type}"
            ));
        }
    }
    Ok(())
}

fn source_row_count(conn: &Connection, table: &SourceTable) -> Result<u64, String> {
    let sql = format!(
        "SELECT COUNT(*) FROM {}.{}",
        quote_ident(&table.schema),
        quote_ident(&table.table)
    );
    let count: i64 = conn
        .query_row(&sql, [], |row| row.get(0))
        .map_err(|error| {
            format!(
                "count source table {}.{} failed: {error}",
                table.schema, table.table
            )
        })?;
    u64::try_from(count).map_err(|_| {
        format!(
            "source table row count is invalid: {}.{}={count}",
            table.schema, table.table
        )
    })
}

fn export_source_table(
    conn: &Connection,
    table: &SourceTable,
    parquet_path: &Path,
) -> Result<(), String> {
    let path = duckdb_path(parquet_path);
    let sql = format!(
        "COPY (SELECT * FROM {}.{}) TO '{}' (FORMAT PARQUET)",
        quote_ident(&table.schema),
        quote_ident(&table.table),
        sql_string(&path)
    );
    conn.execute_batch(&sql).map_err(|error| {
        format!(
            "export source table {}.{} to Parquet failed: {error}",
            table.schema, table.table
        )
    })
}

fn failed_table_report(
    table: &SourceTable,
    source_rows: Option<u64>,
    elapsed_ms: u128,
    error: String,
) -> ImportTableReport {
    ImportTableReport {
        source_schema: table.schema.clone(),
        source_table: table.table.clone(),
        target_table: table.table.clone(),
        status: "failed".into(),
        source_rows,
        target_rows: None,
        elapsed_ms,
        error: Some(error),
    }
}

fn failed_table_report_with_target(
    table: &SourceTable,
    source_rows: u64,
    target_rows: u64,
    elapsed_ms: u128,
    error: String,
) -> ImportTableReport {
    let mut report = failed_table_report(table, Some(source_rows), elapsed_ms, error);
    report.target_rows = Some(target_rows);
    report
}

fn unknown_table_report(
    table: &SourceTable,
    source_rows: u64,
    elapsed_ms: u128,
    error: String,
) -> ImportTableReport {
    let mut report = failed_table_report(table, Some(source_rows), elapsed_ms, error);
    report.status = "unknown".into();
    report
}

fn write_report(config: &ImportDuckDbConfig, report: &ImportDuckDbReport) -> Result<(), String> {
    let path = config
        .report
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("rsduck-import-{}.json", report.migration_id)));
    let payload = serde_json::to_vec_pretty(report)
        .map_err(|error| format!("serialize import-duckdb report failed: {error}"))?;
    fs::write(&path, payload).map_err(|error| {
        format!(
            "write import-duckdb report failed: {}: {error}",
            path.display()
        )
    })?;
    println!("Report:        {}", path.display());
    Ok(())
}

fn print_summary(report: &ImportDuckDbReport) {
    println!();
    println!("DuckDB import summary");
    println!("Status:    {}", report.status);
    println!("Total:     {}", report.total);
    println!("Succeeded: {}", report.succeeded);
    println!("Failed:    {}", report.failed);
}

fn session_cookie(value: &str) -> Option<String> {
    value
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| value.starts_with("rsduck_session=") && value.len() > 16)
        .map(str::to_string)
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("duplicate import-duckdb option: {flag}"));
    }
    *slot = Some(value);
    Ok(())
}

fn validate_target_identifier(kind: &str, value: &str) -> Result<(), String> {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(format!("import-duckdb target {kind} cannot be empty"));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "import-duckdb target {kind} contains unsupported characters: {value}"
        ));
    }
    if kind == "schema"
        && matches!(
            value.to_ascii_lowercase().as_str(),
            "rsduck_catalog" | "rsduck_internal" | "information_schema" | "pg_catalog"
        )
    {
        return Err(format!("import-duckdb target schema is reserved: {value}"));
    }
    Ok(())
}

fn has_fixed_array(data_type: &str) -> bool {
    let bytes = data_type.as_bytes();
    bytes
        .windows(2)
        .any(|window| window[0] == b'[' && window[1].is_ascii_digit())
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn duckdb_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
}

fn path_for_http(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn new_migration_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "job-{}-{}-{nanos}",
        Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DbConfig, VectorApiLimitsConfig};
    use crate::db::{DbHandle, SqlTypedResult};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_directory(label: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rsduck-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_required_and_optional_arguments_without_source_schema() {
        let args = vec![
            "--source".into(),
            "source.duckdb".into(),
            "--target-schema".into(),
            "agent_crm".into(),
            "--password".into(),
            "admin".into(),
            "--tables".into(),
            "main.memory,audit.events".into(),
            "--dry-run".into(),
        ];
        let config = parse_import_duckdb_args(&args).unwrap();
        assert_eq!(config.source, PathBuf::from("source.duckdb"));
        assert_eq!(config.target_schema, "agent_crm");
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, "admin");
        assert_eq!(config.tables, ["main.memory", "audit.events"]);
        assert!(config.dry_run);
    }

    #[test]
    fn rejects_source_schema_and_missing_plaintext_password() {
        let with_source_schema = vec![
            "--source".into(),
            "source.duckdb".into(),
            "--source-schema".into(),
            "main".into(),
            "--target-schema".into(),
            "target".into(),
            "--password".into(),
            "admin".into(),
        ];
        assert!(parse_import_duckdb_args(&with_source_schema)
            .err()
            .unwrap()
            .contains("unknown"));

        let missing_password = vec![
            "--source".into(),
            "source.duckdb".into(),
            "--target-schema".into(),
            "target".into(),
        ];
        assert!(parse_import_duckdb_args(&missing_password)
            .err()
            .unwrap()
            .contains("--password"));
    }

    #[test]
    fn discovers_all_user_schemas_and_marks_flattened_name_conflicts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE SCHEMA alpha;
             CREATE SCHEMA beta;
             CREATE TABLE alpha.memory(id BIGINT);
             CREATE TABLE beta.memory(id BIGINT);
             CREATE TABLE beta.events(id BIGINT);",
        )
        .unwrap();
        let mut tables = discover_source_tables(&conn).unwrap();
        assert_eq!(tables.len(), 3);
        mark_duplicate_targets(&mut tables);
        let conflicts = tables
            .iter()
            .filter(|table| table.table == "memory")
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts
            .iter()
            .all(|table| table.preflight_error.is_some()));
        assert!(tables
            .iter()
            .find(|table| table.table == "events")
            .unwrap()
            .preflight_error
            .is_none());
    }

    #[test]
    fn fixed_arrays_are_rejected_before_parquet_export() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE vectors(id BIGINT, embedding FLOAT[3]);
             CREATE TABLE ordinary(id BIGINT, tags VARCHAR[]);",
        )
        .unwrap();
        let mut tables = discover_source_tables(&conn).unwrap();
        mark_unsupported_tables(&conn, &mut tables).unwrap();
        assert!(tables
            .iter()
            .find(|table| table.table == "vectors")
            .unwrap()
            .preflight_error
            .is_some());
        assert!(tables
            .iter()
            .find(|table| table.table == "ordinary")
            .unwrap()
            .preflight_error
            .is_none());
    }

    #[test]
    fn session_cookie_is_reduced_to_cookie_pair() {
        assert_eq!(
            session_cookie("rsduck_session=abc123; Path=/; HttpOnly"),
            Some("rsduck_session=abc123".into())
        );
        assert_eq!(session_cookie("theme=light; Path=/"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn imports_each_table_independently_and_keeps_partial_success() {
        let root = test_directory("duckdb-import-e2e");
        let source_path = root.join("source.duckdb");
        let import_root = root.join("import-root");
        let report_path = root.join("report.json");
        fs::create_dir_all(&import_root).unwrap();

        {
            let source = Connection::open(&source_path).unwrap();
            source
                .execute_batch(
                    "CREATE SCHEMA alpha;
                     CREATE SCHEMA beta;
                     CREATE TABLE alpha.a_blob_bad(payload BLOB);
                     CREATE TABLE alpha.existing(id BIGINT);
                     INSERT INTO alpha.existing VALUES (99);
                     CREATE TABLE alpha.good(id BIGINT, name VARCHAR);
                     INSERT INTO alpha.good VALUES (1, 'one'), (2, 'two');
                     CREATE TABLE alpha.same(id BIGINT);
                     INSERT INTO alpha.same VALUES (1);
                     CREATE TABLE beta.same(id BIGINT);
                     INSERT INTO beta.same VALUES (2);
                     CREATE TABLE beta.vector_data(id BIGINT, embedding FLOAT[3]);
                     INSERT INTO beta.vector_data VALUES (1, [0.1, 0.2, 0.3]);
                     CREATE TABLE beta.z_after(id BIGINT);
                     INSERT INTO beta.z_after VALUES (10), (20), (30);",
                )
                .unwrap();
        }

        let db = DbHandle::open(
            None,
            &DbConfig {
                vss_enabled: false,
                ..DbConfig::default()
            },
        );
        db.execute_typed_sql_as("admin".into(), "CREATE SCHEMA imported".into())
            .await
            .unwrap();
        db.execute_typed_sql_as(
            "admin".into(),
            "CREATE TABLE imported.existing(id BIGINT)".into(),
        )
        .await
        .unwrap();
        let app = crate::server::web_router(
            db.clone(),
            String::new(),
            String::new(),
            import_root.display().to_string(),
            vec![],
            VectorApiLimitsConfig::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = ImportDuckDbConfig {
            source: source_path,
            target_schema: "imported".into(),
            endpoint: format!("http://{address}"),
            username: "admin".into(),
            password: "admin".into(),
            tables: vec![],
            dry_run: false,
            keep_temp: false,
            report: Some(report_path.clone()),
            if_exists: "error".into(),
        };
        let report = tokio::task::spawn_blocking(move || run_import_duckdb(config))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(report.total, 7);
        assert_eq!(report.succeeded, 2);
        assert_eq!(report.failed, 5);
        assert_eq!(report.exit_code(), 2);
        assert!(report_path.is_file());
        assert!(report.tables.iter().any(|table| {
            table.source_schema == "alpha"
                && table.source_table == "good"
                && table.status == "succeeded"
        }));
        assert!(report.tables.iter().any(|table| {
            table.source_schema == "beta"
                && table.source_table == "z_after"
                && table.status == "succeeded"
        }));
        assert_eq!(
            query_count(&db, "SELECT COUNT(*) FROM imported.good").await,
            2
        );
        assert_eq!(
            query_count(&db, "SELECT COUNT(*) FROM imported.z_after").await,
            3
        );
        assert_eq!(
            query_count(&db, "SELECT COUNT(*) FROM imported.existing").await,
            0
        );
        assert!(db
            .execute_typed_sql_as("admin".into(), "SELECT * FROM imported.same".into())
            .await
            .is_err());
        assert!(db
            .execute_typed_sql_as("admin".into(), "SELECT * FROM imported.a_blob_bad".into())
            .await
            .is_err());
        assert!(db
            .execute_typed_sql_as("admin".into(), "SELECT * FROM imported.vector_data".into())
            .await
            .is_err());

        server.abort();
        db.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    async fn query_count(db: &DbHandle, sql: &str) -> i64 {
        let result = db
            .execute_typed_sql_as("admin".into(), sql.to_string())
            .await
            .unwrap();
        let SqlTypedResult::Query { rows, .. } = result else {
            panic!("expected query result");
        };
        rows[0][0].text_value().unwrap().parse().unwrap()
    }
}

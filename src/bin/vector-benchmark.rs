use duckdb::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug)]
struct Config {
    rows: usize,
    dimension: usize,
    queries: usize,
    top_k: Vec<usize>,
    mutation_rows: usize,
    m: usize,
    m0: usize,
    ef_construction: usize,
    ef_search: usize,
    extension_dir: PathBuf,
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Report {
    duckdb_version: String,
    vss_version: String,
    rows: usize,
    dimension: usize,
    queries: usize,
    m: usize,
    m0: usize,
    ef_construction: usize,
    ef_search: usize,
    load_ms: f64,
    build_ms: f64,
    peak_rss_after_build_bytes: Option<u64>,
    searches: Vec<SearchReport>,
    mutation_rows: usize,
    insert_ms: f64,
    delete_ms: f64,
    compact_ms: f64,
    rebuild_ms: f64,
    peak_rss_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SearchReport {
    top_k: usize,
    recall: f64,
    exact_latency_ms: LatencyReport,
    ann_latency_ms: LatencyReport,
}

#[derive(Debug, Serialize)]
struct LatencyReport {
    p50: f64,
    p95: f64,
    p99: f64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("vector benchmark failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = parse_args()?;
    validate_config(&config)?;
    fs::create_dir_all(&config.extension_dir)
        .map_err(|error| format!("create extension directory failed: {error}"))?;

    let conn = Connection::open_in_memory()
        .map_err(|error| format!("open benchmark database failed: {error}"))?;
    let extension_dir = sql_string(&config.extension_dir.display().to_string());
    conn.execute_batch(&format!(
        "SET extension_directory = '{extension_dir}'; INSTALL vss; LOAD vss;"
    ))
    .map_err(|error| format!("prepare VSS extension failed: {error}"))?;
    let vss_version: String = conn
        .query_row(
            "SELECT COALESCE(extension_version, '') FROM duckdb_extensions() WHERE extension_name = 'vss'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("read VSS version failed: {error}"))?;
    let duckdb_version: String = conn
        .query_row("SELECT version()", [], |row| row.get(0))
        .map_err(|error| format!("read DuckDB version failed: {error}"))?;

    let load_started = Instant::now();
    conn.execute_batch(&format!(
        "CREATE TABLE vector_benchmark (
            tenant_id BIGINT NOT NULL,
            agent_id BIGINT NOT NULL,
            memory_id BIGINT NOT NULL,
            source_version BIGINT NOT NULL,
            content_hash VARCHAR NOT NULL,
            embedding FLOAT[{}] NOT NULL,
            updated_at TIMESTAMP NOT NULL
         );
         INSERT INTO vector_benchmark
         SELECT 1, 1, i, 1, CAST(i AS VARCHAR), {}, CURRENT_TIMESTAMP
         FROM range(1, {}) generated(i);",
        config.dimension,
        generated_vector_sql("i", config.dimension),
        config.rows + 1
    ))
    .map_err(|error| format!("generate benchmark vectors failed: {error}"))?;
    let load_ms = elapsed_ms(load_started);

    let create_index_sql = format!(
        "CREATE INDEX vector_benchmark_hnsw ON vector_benchmark USING HNSW (embedding)
         WITH (metric = 'cosine', ef_construction = {}, M = {}, M0 = {})",
        config.ef_construction, config.m, config.m0
    );
    let build_started = Instant::now();
    conn.execute_batch(&create_index_sql)
        .map_err(|error| format!("build benchmark HNSW index failed: {error}"))?;
    let build_ms = elapsed_ms(build_started);
    let peak_rss_after_build_bytes = peak_rss_bytes();

    let query_ids = query_ids(config.rows, config.queries);
    let mut searches = Vec::with_capacity(config.top_k.len());
    for top_k in &config.top_k {
        searches.push(run_search_benchmark(
            &conn,
            &query_ids,
            *top_k,
            config.dimension,
            config.ef_search,
        )?);
    }

    let insert_started = Instant::now();
    if config.mutation_rows > 0 {
        conn.execute_batch(&format!(
            "INSERT INTO vector_benchmark
             SELECT 1, 1, i, 1, CAST(i AS VARCHAR), {}, CURRENT_TIMESTAMP
             FROM range({}, {}) generated(i);",
            generated_vector_sql("i", config.dimension),
            config.rows + 1,
            config.rows + config.mutation_rows + 1
        ))
        .map_err(|error| format!("benchmark batch insert failed: {error}"))?;
    }
    let insert_ms = elapsed_ms(insert_started);

    let delete_started = Instant::now();
    if config.mutation_rows > 0 {
        conn.execute_batch(&format!(
            "DELETE FROM vector_benchmark WHERE memory_id > {}",
            config.rows
        ))
        .map_err(|error| format!("benchmark batch delete failed: {error}"))?;
    }
    let delete_ms = elapsed_ms(delete_started);

    let compact_started = Instant::now();
    conn.execute_batch("PRAGMA hnsw_compact_index('vector_benchmark_hnsw')")
        .map_err(|error| format!("compact benchmark HNSW index failed: {error}"))?;
    let compact_ms = elapsed_ms(compact_started);

    let rebuild_started = Instant::now();
    conn.execute_batch("DROP INDEX vector_benchmark_hnsw")
        .and_then(|_| conn.execute_batch(&create_index_sql))
        .map_err(|error| format!("rebuild benchmark HNSW index failed: {error}"))?;
    let rebuild_ms = elapsed_ms(rebuild_started);

    let report = Report {
        duckdb_version,
        vss_version,
        rows: config.rows,
        dimension: config.dimension,
        queries: query_ids.len(),
        m: config.m,
        m0: config.m0,
        ef_construction: config.ef_construction,
        ef_search: config.ef_search,
        load_ms,
        build_ms,
        peak_rss_after_build_bytes,
        searches,
        mutation_rows: config.mutation_rows,
        insert_ms,
        delete_ms,
        compact_ms,
        rebuild_ms,
        peak_rss_bytes: peak_rss_bytes(),
    };
    let json = serde_json::to_string_pretty(&report)
        .map_err(|error| format!("serialize benchmark report failed: {error}"))?;
    if let Some(output) = &config.output {
        fs::write(output, format!("{json}\n"))
            .map_err(|error| format!("write benchmark report failed: {error}"))?;
        println!("benchmark report written to {}", output.display());
    } else {
        println!("{json}");
    }
    Ok(())
}

fn run_search_benchmark(
    conn: &Connection,
    query_ids: &[usize],
    top_k: usize,
    dimension: usize,
    ef_search: usize,
) -> Result<SearchReport, String> {
    let mut exact_latencies = Vec::with_capacity(query_ids.len());
    let mut ann_latencies = Vec::with_capacity(query_ids.len());
    let mut recalled = 0usize;
    let mut expected = 0usize;
    conn.execute_batch(&format!("SET hnsw_ef_search = {ef_search}"))
        .map_err(|error| format!("set hnsw_ef_search failed: {error}"))?;
    for query_id in query_ids {
        let vector: String = conn
            .query_row(
                &format!(
                    "SELECT CAST(embedding AS VARCHAR) FROM vector_benchmark WHERE memory_id = {query_id}"
                ),
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("read benchmark query vector failed: {error}"))?;
        let vector = format!("{vector}::FLOAT[{dimension}]");
        let exact_started = Instant::now();
        let exact = search_ids(conn, "list_cosine_distance", &vector, top_k)?;
        exact_latencies.push(elapsed_ms(exact_started));
        let ann_started = Instant::now();
        let ann = search_ids(conn, "array_cosine_distance", &vector, top_k)?;
        ann_latencies.push(elapsed_ms(ann_started));
        let ann = ann.into_iter().collect::<HashSet<_>>();
        recalled += exact
            .iter()
            .filter(|memory_id| ann.contains(memory_id))
            .count();
        expected += exact.len();
    }
    conn.execute_batch("RESET hnsw_ef_search")
        .map_err(|error| format!("reset hnsw_ef_search failed: {error}"))?;
    Ok(SearchReport {
        top_k,
        recall: if expected == 0 {
            0.0
        } else {
            recalled as f64 / expected as f64
        },
        exact_latency_ms: latency_report(exact_latencies),
        ann_latency_ms: latency_report(ann_latencies),
    })
}

fn search_ids(
    conn: &Connection,
    function: &str,
    vector: &str,
    top_k: usize,
) -> Result<Vec<i64>, String> {
    let sql = format!(
        "SELECT memory_id FROM vector_benchmark
         WHERE tenant_id = 1 AND agent_id = 1
         ORDER BY {function}(embedding, {vector}), memory_id
         LIMIT {top_k}"
    );
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| format!("prepare benchmark search failed: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| format!("execute benchmark search failed: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read benchmark search result failed: {error}"))
}

fn generated_vector_sql(row: &str, dimension: usize) -> String {
    format!(
        "list_transform(range(0, {dimension}), j -> CAST(
            sin(CAST(({row} + 1) * (j + 1) AS DOUBLE) * 0.0001) +
            cos(CAST(({row} + 7) * (j + 3) AS DOUBLE) * 0.00013)
         AS FLOAT))::FLOAT[{dimension}]"
    )
}

fn query_ids(rows: usize, queries: usize) -> Vec<usize> {
    let count = queries.min(rows);
    (0..count)
        .map(|position| 1 + position.saturating_mul(rows) / count)
        .collect()
}

fn latency_report(mut values: Vec<f64>) -> LatencyReport {
    values.sort_by(f64::total_cmp);
    LatencyReport {
        p50: percentile(&values, 0.50),
        p95: percentile(&values, 0.95),
        p99: percentile(&values, 0.99),
    }
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index]
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config {
        rows: 10_000,
        dimension: 384,
        queries: 100,
        top_k: vec![10, 100],
        mutation_rows: 1_000,
        m: 16,
        m0: 32,
        ef_construction: 128,
        ef_search: 64,
        extension_dir: PathBuf::from("extensions"),
        output: None,
    };
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        std::process::exit(0);
    }
    let mut index = 0;
    while index < args.len() {
        let key = &args[index];
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("missing value for {key}"))?;
        match key.as_str() {
            "--rows" => config.rows = parse_usize(key, value)?,
            "--dimension" => config.dimension = parse_usize(key, value)?,
            "--queries" => config.queries = parse_usize(key, value)?,
            "--top-k" => {
                config.top_k = value
                    .split(',')
                    .map(|part| parse_usize(key, part))
                    .collect::<Result<Vec<_>, _>>()?
            }
            "--mutation-rows" => config.mutation_rows = parse_usize(key, value)?,
            "--m" => config.m = parse_usize(key, value)?,
            "--m0" => config.m0 = parse_usize(key, value)?,
            "--ef-construction" => config.ef_construction = parse_usize(key, value)?,
            "--ef-search" => config.ef_search = parse_usize(key, value)?,
            "--extension-dir" => config.extension_dir = PathBuf::from(value),
            "--output" => config.output = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument: {key}")),
        }
        index += 2;
    }
    Ok(config)
}

fn parse_usize(key: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{key} must be a non-negative integer: {value}"))
}

fn validate_config(config: &Config) -> Result<(), String> {
    if config.rows == 0 || config.dimension == 0 || config.queries == 0 {
        return Err("rows, dimension and queries must be greater than zero".into());
    }
    if config.top_k.is_empty()
        || config
            .top_k
            .iter()
            .any(|top_k| *top_k == 0 || *top_k > config.rows)
    {
        return Err("top-k values must be between 1 and rows".into());
    }
    if config.m == 0 || config.m0 < config.m {
        return Err("M must be greater than zero and M0 must be at least M".into());
    }
    if config.ef_construction == 0 || config.ef_search == 0 {
        return Err("ef-construction and ef-search must be greater than zero".into());
    }
    Ok(())
}

fn print_usage() {
    println!(
        "usage: cargo run --release --bin vector-benchmark -- \\
  --rows <count> --dimension <N> --queries <count> --top-k <10,100> \\
  --mutation-rows <count> --m <N> --m0 <N> --ef-construction <N> \\
  --ef-search <N> --extension-dir <path> [--output <report.json>]"
    );
}

#[cfg(windows)]
fn peak_rss_bytes() -> Option<u64> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) == 0
        {
            None
        } else {
            Some(counters.PeakWorkingSetSize as u64)
        }
    }
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        let bytes = usage.ru_maxrss as u64;
        #[cfg(not(target_os = "macos"))]
        let bytes = usage.ru_maxrss as u64 * 1024;
        Some(bytes)
    }
}

#[cfg(not(any(windows, unix)))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_ids_are_evenly_distributed() {
        assert_eq!(query_ids(100, 4), vec![1, 26, 51, 76]);
        assert_eq!(query_ids(2, 10), vec![1, 2]);
    }

    #[test]
    fn percentile_uses_upper_rank() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&values, 0.50), 3.0);
        assert_eq!(percentile(&values, 0.95), 5.0);
    }
}

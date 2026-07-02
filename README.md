# rsduck

Language: English | [简体中文](README.zh-CN.md)

rsduck is an in-memory database middleware service built on DuckDB. It starts an in-process DuckDB memory database, exposes a PostgreSQL wire protocol endpoint and a Web SQL console, and persists the in-memory database through directory-based snapshots.

## Features

- In-memory DuckDB: data is mainly read and written in memory after startup, suitable for low-latency analytical queries.
- PostgreSQL wire protocol: external tools can connect to rsduck through a PG-compatible endpoint.
- Web SQL console: browse tables, execute SQL, page through results, and trigger snapshots from the browser.
- Multi-read single-write architecture: read requests are dispatched to read workers, while write requests go through one write worker to reduce read/write blocking.
- Directory snapshots: uses DuckDB `EXPORT DATABASE` / `IMPORT DATABASE` to save and restore the full database.
- Init SQL: when no snapshot exists, `init.sql` can initialize schema and seed data.
- Load test script: `scripts/rsduck_load_test.py` continuously writes data and runs concurrent queries for testing.
- GitHub Actions: push to the remote repository to run formatting checks, tests, and Windows release builds.

## Use Cases

- Temporary analytical stores with high-frequency writes and real-time queries.
- Lightweight PG-compatible data services without deploying a full database server.
- In-memory analysis for K-line data, factors, logs, metrics, and monitoring data.
- Local development, strategy backtesting, data experiments, and temporary data APIs.
- In-memory database services that need fast startup and low-frequency snapshot persistence.

## Quick Start

Development build:

```powershell
cargo build
```

Release build:

```powershell
cargo build --release
```

In this local workspace, build outputs are usually located at:

```text
D:\cargo-target\debug\rsduck.exe
D:\cargo-target\release\rsduck.exe
```

Start the service:

```powershell
D:\cargo-target\release\rsduck.exe
```

Default endpoints:

```text
PG wire: 127.0.0.1:15432
Web:     http://127.0.0.1:8080
```

## Web Console

The Web console shows database tables on the left, a SQL editor on the top-right, and query results below. It also provides pagination, manual snapshots, and a draggable splitter between the editor and result panel.

![rsduck Web SQL Console](console.png)

## Configuration

The default configuration file is `rsduck.toml`:

```toml
[db]
init_sql = "init.sql"
read_workers = 4
write_queue_size = 100000
read_queue_size = 1024
snapshot_queue_size = 16
max_result_rows = 100000

[snapshot]
restore_on_startup = true
dir = "snapshot"
prefix = "rsduck"
interval_secs = 900
retain_hours = 2

[pg]
bind = "127.0.0.1:15432"

[web]
enabled = true
bind = "127.0.0.1:8080"
```

Startup restore order:

1. If `restore_on_startup = true`, scan for the latest finalized snapshot directory.
2. If a snapshot is found, run `IMPORT DATABASE` to restore the full database.
3. If no snapshot exists, run `db.init_sql`.
4. If `init_sql = ""`, start an empty in-memory database.

## Snapshots

rsduck uses directory snapshots to persist the full DuckDB database:

```text
snapshot/
  rsduck_20260703_120000/
    schema.sql
    load.sql
    table_a.parquet
    table_b.parquet
```

Snapshots are first written to a temporary directory:

```text
snapshot/rsduck_yyyyMMdd_HHmmss.tmp
```

After export succeeds, the temporary directory is renamed to the finalized snapshot directory:

```text
snapshot/rsduck_yyyyMMdd_HHmmss
```

The `Save Snapshot` button in the top-right corner of the Web console can trigger a manual snapshot.

## Example: Real-Time K-Line Writes And Queries

The default `init.sql` creates a `kline_day` table:

```sql
CREATE TABLE IF NOT EXISTS kline_day (
    code      VARCHAR NOT NULL,
    bar_time  TIMESTAMP NOT NULL,
    open      DOUBLE,
    high      DOUBLE,
    low       DOUBLE,
    close     DOUBLE,
    volume    BIGINT,
    PRIMARY KEY (code, bar_time)
);
```

After starting rsduck, open the Web console:

```text
http://127.0.0.1:8080
```

Run the load test script to continuously write rows and execute concurrent queries:

```powershell
python scripts\rsduck_load_test.py --write-interval 0.5 --write-batch 10 --query-workers 4 --query-interval 0.2
```

Run a query in the Web console:

```sql
SELECT * FROM kline_day ORDER BY bar_time DESC LIMIT 100;
```

Inspect table metadata:

```sql
SELECT schema_name, table_name, estimated_size, column_count
FROM duckdb_tables()
WHERE internal = false
ORDER BY schema_name, table_name;
```

## GitHub Actions

The GitHub Actions workflow is located at:

```text
.github/workflows/ci.yml
```

On push or pull request, it runs:

```text
cargo fmt --check
cargo test
cargo build --release
```

It uploads the Windows executable:

```text
target/release/rsduck.exe
```

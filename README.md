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
- GitHub Actions: push to the remote repository to run formatting checks, tests, and multi-platform release builds.

## Use Cases

- Temporary analytical stores with high-frequency writes and real-time queries.
- Lightweight PG-compatible data services without deploying a full database server.
- In-memory analysis for K-line data, factors, logs, metrics, and monitoring data.
- Local development, strategy backtesting, data experiments, and temporary data APIs.
- In-memory database services that need fast startup and low-frequency snapshot persistence.

## Architecture

Architecture design reference: [DuckDB connection pool and single-write multi-read design](doc/duckdb-pool-design.md).

## Quick Start

### 1. Prepare `init.sql`

`init.sql` is the first-start schema script. rsduck executes it only when no snapshot is restored. Put table DDL and optional seed data here:

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

The repository sample `rsduck.toml` points `[db].init_sql` to this file:

```toml
[db]
init_sql = "init.sql"
```

If `rsduck.toml` is missing, the built-in default is `init_sql = ""`, so rsduck starts an empty in-memory database when no snapshot exists.

### 2. Build

Development build:

```powershell
cargo build
```

Release build:

```powershell
cargo build --release
```

Build outputs depend on Cargo's target directory. If `CARGO_TARGET_DIR` is set, artifacts are written under that directory; otherwise they are written under the repository's `target` directory. In this workspace, `CARGO_TARGET_DIR` points to `D:\cargo-target`, so the paths are usually:

```text
D:\cargo-target\debug\rsduck.exe
D:\cargo-target\release\rsduck.exe
```

### 3. Start the service

```powershell
D:\cargo-target\release\rsduck.exe
```

Adjust the executable path to your actual build output location.

Default endpoints:

```text
PG wire: 127.0.0.1:15432
Web:     http://127.0.0.1:8080
```

## Web Console

The Web console shows database tables on the left, a SQL editor on the top-right, and query results below. It also provides pagination, manual snapshots, and a draggable splitter between the editor and result panel.

![rsduck Web SQL Console](console.png)

## Programmatic Access

rsduck exposes two programmatic entry points:

- HTTP SQL API at `http://127.0.0.1:8080/sql`
- PostgreSQL wire protocol at `127.0.0.1:15432`

### HTTP API With Python Standard Library

This example has no third-party Python dependency. It sends complete SQL text to the Web API and can query or write rows:

```python
import json
from urllib.request import Request, urlopen

BASE_URL = "http://127.0.0.1:8080"

def run_sql(sql, page=0, page_size=1000):
    payload = json.dumps({
        "sql": sql,
        "page": page,
        "page_size": page_size,
    }).encode("utf-8")
    req = Request(
        BASE_URL + "/sql",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req, timeout=10) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    if not data["success"]:
        raise RuntimeError(data["msg"])
    return data

run_sql("""
INSERT INTO kline_day
(code, bar_time, open, high, low, close, volume)
VALUES
('600000', TIMESTAMP '2026-07-03 09:30:00', 10.1, 10.5, 9.9, 10.2, 120000)
""")

result = run_sql("SELECT code, close, volume FROM kline_day ORDER BY bar_time DESC LIMIT 10")
print(result["columns"])
print(result["rows"])
```

HTTP request shape:

```json
{
  "sql": "SELECT * FROM kline_day LIMIT 10",
  "page": 0,
  "page_size": 1000
}
```

Response shape:

```json
{
  "columns": ["code", "close"],
  "rows": [["600000", "10.2"]],
  "success": true,
  "msg": "1 row(s)"
}
```

### PostgreSQL Wire Protocol

PG-compatible tools and drivers can connect through the PG wire endpoint. The current adapter does not enforce authentication; `dbname`, `user`, and `password` are compatibility fields, not separate DuckDB databases or users.

Connection values:

```text
host:     127.0.0.1
port:     15432
database: postgres
user:     postgres
password: any value or empty
```

Python example with `psycopg`:

```powershell
pip install "psycopg[binary]"
```

```python
import psycopg

conn = psycopg.connect(
    host="127.0.0.1",
    port=15432,
    dbname="postgres",
    user="postgres",
    password="postgres",
)

with conn:
    with conn.cursor() as cur:
        cur.execute("""
            INSERT INTO kline_day
            (code, bar_time, open, high, low, close, volume)
            VALUES
            ('600001', TIMESTAMP '2026-07-03 09:31:00', 11.0, 11.4, 10.8, 11.2, 90000)
        """)

        cur.execute("SELECT code, close, volume FROM kline_day ORDER BY bar_time DESC LIMIT 10")
        print(cur.fetchall())
```

For long-running load tests, use:

```powershell
python scripts\rsduck_load_test.py --write-interval 0.5 --write-batch 10 --query-workers 4 --query-interval 0.2
```

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

Parameter reference, in `rsduck.toml` order:

- 【db.init_sql】Path to the initialization SQL file. It runs only when startup does not restore a snapshot. Use it to create tables, indexes, views, or seed data. Set it to `""` to start empty.
- 【db.read_workers】Number of dedicated DuckDB read worker threads. Read SQL is distributed across these workers. Increase it for more concurrent reads, while keeping memory and CPU capacity in mind.
- 【db.write_queue_size】Bounded queue size for write SQL. Writes are serialized through the single write worker; when this queue is full, new write requests fail quickly instead of blocking indefinitely.
- 【db.read_queue_size】Bounded queue size for each read worker. When a read queue is full, new read requests fail quickly.
- 【db.snapshot_queue_size】Bounded queue size for snapshot requests from the scheduler, shutdown hook, and Web console. A full queue means another snapshot is already waiting or running.
- 【db.max_result_rows】Maximum number of rows returned by one SQL execution before Web pagination is applied. This protects the service from returning excessively large result sets.
- 【snapshot.restore_on_startup】Whether to restore the latest finalized snapshot directory at startup. If enabled and a matching snapshot exists, `db.init_sql` is not executed.
- 【snapshot.dir】Base directory used to read and write snapshot directories.
- 【snapshot.prefix】Snapshot directory prefix. Final snapshot names use `prefix_yyyyMMdd_HHmmss`, for example `rsduck_20260703_120000`.
- 【snapshot.interval_secs】Automatic snapshot interval in seconds. The scheduler saves one snapshot at this cadence while the service is running.
- 【snapshot.retain_hours】Retention window for old finalized snapshots. Expired snapshot directories are removed after scheduled snapshot cleanup.
- 【pg.bind】Listen address for the PostgreSQL wire endpoint. Keep `127.0.0.1` for local-only access; use an explicit LAN address only when external clients should connect.
- 【web.enabled】Whether to start the Web SQL console.
- 【web.bind】Listen address for the Web console and HTTP SQL API.

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

## GitHub Actions And Downloads

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

Download released builds from:

- Latest release: [github.com/dripai/rsduck/releases/latest](https://github.com/dripai/rsduck/releases/latest)
- All releases: [github.com/dripai/rsduck/releases](https://github.com/dripai/rsduck/releases)
- CI artifacts for each workflow run: [github.com/dripai/rsduck/actions/workflows/ci.yml](https://github.com/dripai/rsduck/actions/workflows/ci.yml)

The workflow packages these files:

```text
rsduck-windows-x64.zip
rsduck-linux-x64.tar.gz
rsduck-macos-arm64.tar.gz
rsduck-macos-x64.tar.gz
```

Workflow run artifacts are temporary CI outputs. GitHub Release downloads are created when a `v*` tag is pushed, for example `v0.1.0`.

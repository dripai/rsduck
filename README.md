# rsduck

Language: English | [简体中文](README.zh-CN.md)

rsduck is a Rust service that wraps DuckDB as an in-memory database middleware. It starts an in-process DuckDB memory database, exposes a PostgreSQL wire protocol endpoint and a Web SQL console, and persists the in-memory database through directory-based snapshots.

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
[log]
level = "info"
dir = "logs"
file_name = "rsduck.log"
retain_files = 3
console = false

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

## Ways To Use

### Web Console

The Web console shows database tables on the left, a SQL editor on the top-right, and query results below. It also provides pagination, manual snapshots, and a draggable splitter between the editor and result panel.

![rsduck Web SQL Console](console.png)

### HTTP SQL API

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
print([column["name"] for column in result["columns"]])
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
  "columns": [
    { "name": "code", "pg_type_oid": 25 },
    { "name": "close", "pg_type_oid": 701 }
  ],
  "rows": [["600000", "10.2"]],
  "success": true,
  "msg": "ok"
}
```

`columns` uses the same column metadata as PG wire, and `pg_type_oid` is the PostgreSQL type OID. SQL `NULL` is returned as JSON `null`; an empty string stays `""`.

### PostgreSQL Wire Protocol

PG-compatible tools and drivers can connect through the PG wire endpoint. PG wire and the Web console share catalog authentication. The default bootstrap administrator is `admin/admin`; change it before production use.

Connection values:

```text
host:     127.0.0.1
port:     15432
database: memory
user:     admin
password: admin
```

Change the password after login:

```sql
ALTER USER admin PASSWORD 'new_password';
```

If the `admin` password is forgotten and no other active admin user exists, run `rsduck reset-admin-password --password <new_password>` while the service is stopped. Without `--password`, the command resets `admin` back to password `admin`. The command obtains `.rsduck.lock`, imports the latest snapshot into a temporary DuckDB connection, resets the password through catalog mutation, and exports a new snapshot. Do not edit snapshot parquet files directly.

Python example with `psycopg`:

```powershell
pip install "psycopg[binary]"
```

```python
import psycopg

conn = psycopg.connect(
    host="127.0.0.1",
    port=15432,
    dbname="memory",
    user="admin",
    password="admin",
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
[log]
level = "info"
dir = "logs"
file_name = "rsduck.log"
retain_files = 3
console = false

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

[partition]
maintenance_enabled = true
maintenance_interval_secs = 60
verify_interval_secs = 300
max_jobs_per_tick = 100

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

- 【log.level】Global logging level. Valid values are `trace`, `debug`, `info`, `warn`, `error`, and `off`. Use `debug` while diagnosing PostgreSQL client compatibility so PG wire connection and SQL events are printed.
- 【log.dir】rsduck application log directory. Relative paths are resolved from the service working directory.
- 【log.file_name】rsduck application log file name. The process writes daily files named `file_name.yyyy-MM-dd`.
- 【log.retain_files】Number of rsduck application log files to keep. The default keeps the latest 3 daily files.
- 【log.console】Whether to also write application logs to stdout. Keep this `false` for Windows service mode to avoid duplicating application logs into the WinSW wrapper logs.
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
- 【partition.maintenance_enabled】Whether the partition scheduler periodically submits maintenance work to the write queue. Writes still create required partitions synchronously.
- 【partition.maintenance_interval_secs】Interval for retention cleanup and partition entrypoint refresh.
- 【partition.verify_interval_secs】Reserved interval for partition verification scans.
- 【partition.max_jobs_per_tick】Reserved limit for maintenance jobs submitted by one scheduler tick.
- 【pg.bind】Listen address for the PostgreSQL wire endpoint. Keep `127.0.0.1` for local-only access; use an explicit LAN address only when external clients should connect.
- 【web.enabled】Whether to start the Web SQL console.
- 【web.bind】Listen address for the Web console and HTTP SQL API.

## Snapshots And Recovery

At startup, when `restore_on_startup = true`, rsduck selects the latest finalized snapshot directory from `snapshot.dir` and runs `IMPORT DATABASE`; `db.init_sql` runs only when no snapshot exists.

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

## Architecture

rsduck wraps DuckDB in a Rust service instead of reimplementing a PostgreSQL kernel. DuckDB remains the only SQL execution engine; rsduck adds network endpoints, a PG-compatible catalog, authentication, execution scheduling, managed range partitions, and snapshot-based recovery around it.

Runtime model:

- One shared in-memory DuckDB database is opened in the process.
- Internal DuckDB connections are cloned from the same base connection with `try_clone()`.
- Read SQL is dispatched to read worker threads, while writes, DDL, catalog mutations, and partition maintenance are serialized through one write worker.
- Snapshot work uses a dedicated snapshot worker and DuckDB `EXPORT DATABASE` / `IMPORT DATABASE` directory snapshots.
- Network services, session handling, scheduled tasks, and the Web console run outside the DuckDB worker threads.

User-facing model:

- Web SQL Console: `http://127.0.0.1:8080`.
- PostgreSQL wire endpoint: `127.0.0.1:15432`.
- HTTP SQL API: `http://127.0.0.1:8080/sql`.
- Default bootstrap administrator: `admin/admin`; change it before production use.
- Business objects are created in the DuckDB default schema `main` unless another schema is explicitly used.
- `pg_catalog.*` and `information_schema.*` are read-only compatibility projections.
- `rsduck_catalog.*` and `rsduck_internal.*` are internal schemas and are not normal application surfaces.

Developer module map:

```text
src/
  main.rs              process startup, schedulers, service lifecycle
  config.rs            configuration loading and defaults
  sql_route.rs         SQL read/write routing

  db/                  DuckDB engine, workers, SQL execution, snapshot, restore
  catalog/             catalog source of truth, auth, mutation, partition, recovery
  pg_compat/           pg_catalog / information_schema rewrite and compatibility
  server/              PostgreSQL wire server and Web server
```

Request flow:

```text
client
  -> Web API or PG wire entry authenticates user and handles protocol encoding
  -> db::execute_typed_sql_as(username, sql) / db::describe_sql_with_params_as(username, sql, params)
  -> sql_route::route_sql
  -> read worker or write worker
  -> pg_compat rewrite if metadata query
  -> catalog guard and authorization
  -> DuckDB execute/query
  -> SqlTypedResult / SqlColumn
  -> Web API JSON or PG RowDescription/DataRow encoding
```

Core design boundaries:

- DuckDB is the only SQL execution engine.
- `rsduck_catalog.*` is the metadata source of truth.
- Web API and PG wire are entry adapters only; after authentication they share the same typed SQL execution and Describe paths.
- Writes, DDL, and catalog mutations must go through the single write worker.
- `pg_catalog.*` and `information_schema.*` are read-only projections derived from rsduck catalog metadata.
- Unsupported compatibility behavior returns a clear error or a defined empty result; rsduck does not silently fall back to DuckDB internal catalog tables.
- Snapshot restore reads only the latest finalized snapshot directory and does not automatically try older snapshots.

Deep-dive design docs:

- [DuckDB connection pool and single-write multi-read design](doc/duckdb-pool-design.md)
- [PG-compatible catalog design](doc/rsduck_pg_catalog_design.md)

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

When a `v*` tag is pushed, it runs:

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
rsduck-windows-service-setup-x64.exe
rsduck-linux-x64.tar.gz
rsduck-macos-arm64.tar.gz
rsduck-macos-x64.tar.gz
```

## Service Registration

### Windows

Download `rsduck-windows-service-setup-x64.exe` from Releases. It is the easiest Windows package: double-click it, choose the install directory, and the installer registers rsduck as an automatic Windows service.

The installer places `rsduck.exe`, `rsduck.toml`, `init.sql`, WinSW service files, `logs`, and `snapshot` under the selected directory. That directory is also used as the service working directory.

rsduck application logs are written to `logs\rsduck.log.yyyy-MM-dd` and keep the latest 3 rotated files by default. WinSW stdout/stderr wrapper logs are also limited to 3 files and normally only contain launcher output, panics, or stderr not handled by application logging.

For portable console usage without service registration, use `rsduck-windows-x64.zip`.

Service commands:

```powershell
Get-Service rsduck
Start-Service rsduck
Stop-Service rsduck
```

Uninstall from Windows Apps/Programs, or use the Start Menu item `Uninstall rsduck`.

### Linux

Place the release files under `/opt/rsduck`:

```bash
sudo mkdir -p /opt/rsduck
sudo tar -xzf rsduck-linux-x64.tar.gz -C /opt/rsduck
sudo cp rsduck.toml init.sql /opt/rsduck/
```

Create `/etc/systemd/system/rsduck.service`:

```ini
[Unit]
Description=rsduck in-memory DuckDB middleware service
After=network.target

[Service]
Type=simple
WorkingDirectory=/opt/rsduck
ExecStart=/opt/rsduck/rsduck
Restart=always
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

Enable and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable rsduck
sudo systemctl start rsduck
sudo systemctl status rsduck
```

### macOS

Place the release files under `/usr/local/rsduck`:

```bash
sudo mkdir -p /usr/local/rsduck
sudo tar -xzf rsduck-macos-arm64.tar.gz -C /usr/local/rsduck
sudo cp rsduck.toml init.sql /usr/local/rsduck/
```

On Intel macOS, use `rsduck-macos-x64.tar.gz` instead.

Create `/Library/LaunchDaemons/com.rsduck.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.rsduck</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/rsduck/rsduck</string>
  </array>
  <key>WorkingDirectory</key>
  <string>/usr/local/rsduck</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/usr/local/rsduck/rsduck.out.log</string>
  <key>StandardErrorPath</key>
  <string>/usr/local/rsduck/rsduck.err.log</string>
</dict>
</plist>
```

Load and start it:

```bash
sudo chown root:wheel /Library/LaunchDaemons/com.rsduck.plist
sudo chmod 644 /Library/LaunchDaemons/com.rsduck.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.rsduck.plist
sudo launchctl enable system/com.rsduck
sudo launchctl kickstart -k system/com.rsduck
```

Stop and unload:

```bash
sudo launchctl bootout system /Library/LaunchDaemons/com.rsduck.plist
```

For graceful shutdown snapshots, rsduck currently handles Ctrl+C/SIGINT. The Linux `systemd` example sends SIGINT. On macOS, take a manual snapshot before `launchctl bootout` if the latest in-memory changes must be persisted immediately.

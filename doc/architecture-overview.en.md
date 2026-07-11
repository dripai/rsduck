# rsduck Architecture Overview

Language: English | [中文](architecture-overview.md)

This document describes rsduck from an architecture perspective: how the process starts, how the in-memory DuckDB instance is wrapped as a service, how threads and workers cooperate, and how the Web and MySQL entry points enter the same execution engine.

## 1. Design Goals

rsduck does not expose DuckDB directly to external clients. It wraps DuckDB as a controlled in-memory database service:

- Exposes a MySQL wire protocol endpoint for Navicat and MySQL clients.
- Exposes a Web SQL console for query execution, snapshots, and Parquet table import.
- Uses `rsduck_catalog.rs_*` to manage objects, privileges, dependencies, and snapshot metadata.
- Persists all recoverable state through Snapshot v2.
- Returns explicit errors for unsupported capabilities instead of silently falling back to DuckDB internal catalog tables or older paths.

The high-level architecture is:

```text
                 +---------------------+
                 |      rsduck.exe      |
                 +----------+----------+
                            |
                    load config / lock
                            |
                    restore or bootstrap
                            |
                +-----------+-----------+
                |    in-memory DuckDB   |
                +-----------+-----------+
                            |
       +--------------------+--------------------+
       |                    |                    |
  Web service          MySQL service       background jobs
  Axum HTTP            MySQL wire          snapshot / partition
       |                    |                    |
       +----------+---------+--------------------+
                  |
             DbHandle API
                  |
       +----------+----------+
       |                     |
  read worker pool      write worker
       |                     |
       +----------+----------+
                  |
          DuckDB cloned connections
```

## 2. Startup Flow

The main entry point is `src/main.rs`. Startup follows a fixed order:

1. Read `rsduck.toml` and initialize logging.
2. Validate the snapshot prefix to avoid unsafe snapshot directory names.
3. Acquire the `.rsduck.lock` process lock to prevent multiple instances in the same working directory.
4. If `snapshot.restore_on_startup` is enabled, locate the latest Snapshot v2.
5. Call `DbHandle::open` to create the in-memory DuckDB instance and restore or initialize it.
6. Start the partition maintenance task if configured.
7. Start the MySQL wire service.
8. Start the periodic snapshot task.
9. Start the Axum Web service if Web is enabled; otherwise wait for shutdown.
10. On shutdown, save one final shutdown snapshot and stop the workers.

The working directory is the runtime state boundary. `rsduck.toml`, `init.sql`, `snapshot/`, `logs/`, `.rsduck.lock`, and `.rsduck.lock.guard` are all resolved from the current working directory. Windows service deployment must set the working directory explicitly, otherwise rsduck may read the wrong config, miss snapshots, or create lock files in the wrong place.

## 3. Initialization and Restore

`DbHandle::open` creates a base in-memory DuckDB connection, then runs restore or initialization:

- If a snapshot directory is provided, restore catalog, schemas, ordinary table data, indexes, views, macros, and functions from Snapshot v2.
- If no usable snapshot exists, create a fresh `rsduck_catalog`.
- If a fresh database has `init.sql`, execute initialization SQL.

`init.sql` is an internal initialization entry point and may contain multiple statements. DDL in `init.sql` should still go through catalog-aware mutation and should not bypass catalog registration for business objects.

Restore order follows one rule: restore the source of truth first, then restore physical objects.

```text
catalog.duckdb
    -> schema
    -> ordinary table data
    -> indexes
    -> views
    -> macros/functions
    -> checksum and consistency checks
```

If the manifest, catalog checksum, format version, view checksum, or macro checksum does not match, restore fails. If business data files are missing, the corresponding relation is marked unavailable instead of being presented as a healthy object.

## 4. DuckDB Connection Model

rsduck uses one base in-memory DuckDB instance and creates multiple same-origin connections through `Connection::try_clone()`:

- One write connection.
- N read connections.
- One snapshot connection.
- One base connection retained by `DbEngine`.

These connections point to the same in-memory database instance, but each connection still has connection-local state. Therefore:

- External requests should not rely on temporary tables surviving across requests.
- External explicit transactions should not span Web or MySQL requests.
- Two queries from the same user are not guaranteed to land on the same read worker.
- Internal code that needs temporary objects should keep the whole compound operation inside one worker and one connection.

## 5. Workers and Thread Services

Network requests enter rsduck through Tokio-based async services, while DuckDB execution is performed inside thread workers. This avoids blocking the Tokio runtime and preserves DuckDB's synchronous connection model.

### 5.1 Write Worker

The write worker is a single-threaded serialized execution entry point. It handles:

- DDL.
- DML.
- `COPY FROM`.
- User, role, and privilege management.
- Parquet import.
- Web/MySQL authentication queries.

Write operations enter the bounded `write_tx` queue. If the queue is full, rsduck returns a queue-full error and does not route the operation through another path. This keeps write ordering clear and makes catalog mutation atomicity easier to maintain.

### 5.2 Read Worker Pool

The read worker pool contains `db.read_workers` threads. Read-only queries are assigned by round-robin:

```text
next_read % read_workers
```

Typical read-routed statements include:

- `SELECT`
- `WITH`
- `EXPLAIN`
- `SHOW ...`
- `DESCRIBE`
- `COPY ... TO`

Read workers do not hold the snapshot/write gate. They serve query throughput and do not perform catalog mutation.

### 5.3 Snapshot Worker

The snapshot worker uses its own connection and queue to save Snapshot v2.

The snapshot worker and write worker share one `snapshot_write_gate`:

```text
write worker ----+
                 +---- snapshot_write_gate
snapshot worker -+
```

Writes and snapshots cannot run at the same time. This guarantees that exported catalog metadata and business data come from one stable state. Read queries may continue through read workers, but the external consistency boundary is defined by serialized writes and snapshots.

### 5.4 Partition Maintenance Task

If `partition.maintenance_enabled` is enabled, the main process starts a periodic partition maintenance task. It calls:

```sql
CALL rsduck_run_partition_maintenance()
```

This call uses the write route because maintenance may create, mark, or clean managed partitions.

## 6. Internal Command Model

Network layers do not directly hold DuckDB connections. All operations go through `DbHandle` APIs:

```text
execute_typed_sql_as
execute_typed_sql_with_params_as
describe_sql_with_params_as
save_snapshot_as
import_parquet_tables_as
authenticate
run_partition_maintenance
```

These APIs are translated into worker commands:

```text
SqlCommand::RunTyped
SqlCommand::Describe
SqlCommand::Authenticate
SqlCommand::ImportParquet
SqlCommand::Shutdown

SnapshotCommand::Save
SnapshotCommand::Shutdown
```

Commands are sent through bounded channels. Results are returned through oneshot channels. Worker execution is wrapped with `catch_unwind` so a DuckDB or business-code panic does not directly kill the caller task.

## 7. SQL Routing

External SQL enters `route_sql` before execution:

1. Parse SQL using sqlparser with the DuckDB dialect.
2. Require exactly one statement per request.
3. Decide read or write routing based on the statement type.
4. Produce a command name for result reporting and authorization.

Basic read/write classification:

```text
SELECT / WITH / SHOW / DESCRIBE / EXPLAIN / COPY TO -> read
INSERT / UPDATE / DELETE / DDL / GRANT / REVOKE / CALL / COPY FROM -> write
```

Rejecting multi-statement SQL is a product constraint, not a DuckDB limitation. It simplifies:

- Routing.
- Authorization.
- Web single-result response shape.
- Current MySQL protocol implementation.
- Temporary object and transaction boundaries.

## 8. Web Request Path

The Web service is built with Axum. Main endpoints include:

```text
GET  /                  Web page
POST /login             login
POST /logout            logout
GET  /session           current session
POST /sql               query or execute SQL
POST /snapshot          save manual snapshot
GET  /parquet-import    read Parquet import root
POST /parquet-import    import Parquet tables
```

The Web path is:

```text
browser
  -> Axum route
  -> session cookie check
  -> DbHandle
  -> route_sql
  -> worker queue
  -> DuckDB
  -> typed result
  -> JSON response
```

Web sessions use HttpOnly and SameSite=Lax cookies. `POST /sql` automatically wraps `SELECT/WITH` statements without top-level `LIMIT/OFFSET` for pagination:

```sql
SELECT * FROM (<user_sql>) __rsduck_page LIMIT <page_size> OFFSET <offset>
```

SQL that already has explicit pagination is left unchanged. Non-query statements are not paginated.

## 9. MySQL Request Path

The MySQL service listens on TCP and handles MySQL handshake, authentication, and command loops. It mainly supports:

- `COM_QUERY`
- `COM_STMT_PREPARE`
- `COM_STMT_EXECUTE`
- `COM_STMT_CLOSE`
- `COM_STMT_RESET`
- `COM_PING`
- `COM_INIT_DB`

The MySQL path is:

```text
Navicat / MySQL client
  -> TCP listener
  -> handshake
  -> authentication
  -> command loop
  -> MySQL SQL rewrite / metadata projection
  -> DbHandle
  -> worker queue
  -> DuckDB
  -> MySQL packets
```

The MySQL session stores the current database, username, and prepared statements. `COM_INIT_DB` changes the session database. Empty database and `memory` map to `main`.

## 10. Snapshot Path

Snapshots can be triggered by:

- Periodic task.
- Manual Web action.
- Normal shutdown.

Save flow:

```text
trigger
  -> SnapshotCommand::Save
  -> snapshot worker
  -> acquire snapshot_write_gate
  -> optional authorize_snapshot
  -> export catalog.duckdb and data/*.parquet
  -> write manifest.json
  -> atomic rename tmp directory
  -> cleanup retention
```

Manual Web snapshots carry a username and run `authorize_snapshot`. Periodic and shutdown snapshots use system identity.

## 11. Parquet Import Path

Parquet import is exposed only through the Web entry point, not as ordinary external SQL:

```text
POST /parquet-import
  -> session check
  -> resolve path under web.parquet_import_root
  -> build ParquetImportSource list
  -> SqlCommand::ImportParquet
  -> write worker
  -> prepare parquet extension
  -> catalog-aware import
```

Import rules:

- One Parquet file maps to one ordinary table.
- Directory mode imports all top-level `.parquet` files.
- Directory mode is one-file-one-table; it does not automatically union files.
- Batch import is atomic. If any file fails, the whole batch rolls back.
- Imported tables enter `rsduck_catalog` and are managed by privileges, snapshots, and Navicat metadata.

## 12. Downstream Storage Boundary

rsduck's downstream storage consists only of in-memory DuckDB and snapshot files. It does not rely on external database system tables and does not treat MySQL metadata as a source of truth.

Runtime state:

```text
in-memory DuckDB
  -> business schemas and objects
  -> rsduck_catalog.rs_*
  -> rsduck_internal physical partitions
```

Persisted state:

```text
snapshot/
  -> catalog.duckdb
  -> data/*.parquet
  -> manifest.json
```

Logs and locks:

```text
logs/
.rsduck.lock
.rsduck.lock.guard
```

## 13. Shutdown Flow

On shutdown signal:

1. Web graceful shutdown starts.
2. Save a shutdown snapshot.
3. Abort the periodic snapshot task.
4. Abort the partition maintenance task.
5. Abort the MySQL task.
6. Send shutdown commands to write/read/snapshot workers.
7. Join worker threads.
8. Release the process lock.

Forced process termination may skip the shutdown snapshot and may leave lock files behind. During recovery, first confirm whether the recorded PID still exists.

## 14. Architecture Constraints

Future development must preserve these constraints:

- External entry points execute only one SQL statement per request.
- External requests do not support cross-request transactions or temporary table reuse.
- All DDL must go through catalog-aware mutation.
- Writes and snapshots must be serialized.
- MySQL and Web are different entry points, not different execution semantics.
- Do not add silent fallbacks for missing capabilities.
- All recoverable state must enter Snapshot v2.

If any of these constraints need to change, update the product contract, documentation, and tests before changing the implementation.

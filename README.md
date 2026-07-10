# rsduck

Language: English | [简体中文](README.zh-CN.md)

rsduck is an in-memory DuckDB service with a MySQL-compatible wire protocol and Web SQL console. Its metadata source of truth is the `rsduck_catalog.rs_*` catalog; `information_schema` and `SHOW ...` are controlled MySQL-compatible projections.

## Quick Start

```powershell
cargo build --release
D:\cargo-target\release\rsduck.exe
```

Default endpoints:

```text
MySQL: 127.0.0.1:13306
Web:   http://127.0.0.1:8080
```

The bootstrap administrator is `admin/admin`. Change it after startup:

```sql
ALTER USER admin PASSWORD 'new_password';
```

The MySQL configuration has one direct path:

```toml
[mysql]
bind = "127.0.0.1:13306"
```

## MySQL Protocol

The MySQL endpoint supports authentication, queries, prepared statements, `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX`, and common `information_schema` probes. Unsupported metadata relations return a clear error; they never fall back to DuckDB internal catalog tables.

The Web SQL API exposes neutral and MySQL display types:

```json
{
  "columns": [
    { "name": "code", "sql_type": "text", "mysql_type": "varchar" }
  ],
  "rows": [["600000"]],
  "success": true,
  "msg": "ok"
}
```

## Snapshot v2

Snapshots contain only catalog metadata and business data:

```text
snapshot/
  rsduck_20260703_120000/
    manifest.json
    catalog.duckdb
    data/
      10000.parquet
```

`manifest.json` records catalog epoch/checksum, relation data files and row counts, plus DuckDB-derived view and macro DDL with checksums. Restore order is catalog, schema, business data, indexes, views, then macros; a damaged object DDL checksum fails restore explicitly.

Startup restores only Snapshot v2. Legacy `EXPORT DATABASE` directories are not read automatically; migrate them explicitly:

```powershell
rsduck migrate-snapshot --from <legacy_snapshot_dir> --to <snapshot_dir>
```

Reset the administrator password offline:

```powershell
rsduck reset-admin-password --password <new_password>
```

## Configuration

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

[mysql]
bind = "127.0.0.1:13306"

[web]
enabled = true
bind = "127.0.0.1:8080"
```

# rsduck

语言：[English](README.md) | 简体中文

rsduck 是基于 DuckDB 的内存数据库服务，对外提供 MySQL wire 协议与 Web SQL 控制台。内部元数据只保存在 `rsduck_catalog.rs_*` 表中，`information_schema` 和 `SHOW ...` 由 MySQL-compatible 投影生成。

## 快速开始

```powershell
cargo build --release
D:\cargo-target\release\rsduck.exe
```

默认端点：

```text
MySQL: http://127.0.0.1:13306
Web:   http://127.0.0.1:8080
```

默认管理员为 `admin/admin`。服务启动后请立即修改密码：

```sql
ALTER USER admin PASSWORD 'new_password';
```

`rsduck.toml` 的 MySQL 配置只保留监听地址：

```toml
[mysql]
bind = "127.0.0.1:13306"
```

## MySQL 协议

MySQL wire 支持认证、普通查询、prepared statement、`SHOW TABLES`、`SHOW COLUMNS`、`SHOW INDEX` 以及 `information_schema` 基础探测。`information_schema` 不会回退到 DuckDB 内部 catalog；未支持的 relation 返回明确错误。

Web SQL API 返回中性类型与 MySQL 展示类型：

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

快照目录只包含 catalog 和业务数据：

```text
snapshot/
  rsduck_20260703_120000/
    manifest.json
    catalog.duckdb
    data/
      10000.parquet
```

`manifest.json` 记录 catalog epoch/checksum、relation 数据文件和行数。保存快照时会与写入串行，并在导出结束后再次校验 catalog epoch；变化时本次快照失败并清理临时目录。

启动时只恢复 Snapshot v2。旧 `EXPORT DATABASE` 目录不会自动加载，需要显式迁移：

```powershell
rsduck migrate-snapshot --from <legacy_snapshot_dir> --to <snapshot_dir>
```

离线重置管理员密码：

```powershell
rsduck reset-admin-password --password <new_password>
```

## 配置

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

# rsduck

语言: [English](README.md) | 简体中文

rsduck 是一个基于 DuckDB 的内存数据库中间件服务。它在进程内启动 DuckDB 内存库，对外提供 PostgreSQL wire 协议和 Web SQL 控制台，并通过目录快照实现内存库的持久化恢复。

## 功能概览

- 内存 DuckDB：启动后数据主要在内存中读写，适合低延迟分析查询。
- PostgreSQL wire 接入：外部工具可按 PG 协议连接 rsduck。
- Web SQL 控制台：浏览器中查看表列表、执行 SQL、分页查看结果、手工保存快照。
- 多读单写架构：读请求分发到 read workers，写请求进入 single write worker，降低读写互相阻塞。
- 目录快照：使用 DuckDB `EXPORT DATABASE` / `IMPORT DATABASE` 保存和恢复完整库。
- 初始化 SQL：没有快照时可通过 `init.sql` 初始化表结构。
- 压测脚本：`scripts/rsduck_load_test.py` 可持续写入并发查询，用于观察前端查询影响。
- GitHub Actions：远程 push 后会自动执行格式检查、测试和 Windows release 编译。

## 应用场景

- 高频写入、实时查询的临时分析库。
- 需要 PG 协议入口，但不想部署完整数据库服务的轻量场景。
- 股票 K 线、指标、日志、监控数据等内存分析查询。
- 本地研发、策略回测、数据实验、临时数据服务。
- 需要快速恢复的内存数据库服务，允许低频快照持久化。

## 快速开始

开发编译：

```powershell
cargo build
```

正式编译：

```powershell
cargo build --release
```

当前环境的构建产物通常在：

```text
D:\cargo-target\debug\rsduck.exe
D:\cargo-target\release\rsduck.exe
```

启动服务：

```powershell
D:\cargo-target\release\rsduck.exe
```

默认端口：

```text
PG wire: 127.0.0.1:15432
Web:     http://127.0.0.1:8080
```

## Web 控制台

Web 控制台左侧展示数据库表列表，右侧上方是 SQL 编辑区，下方是查询结果区。页面支持分页、手工保存快照，以及编辑区和结果区之间的拖动分割条。

![rsduck Web SQL 控制台](console.png)

## 配置

默认配置文件为 `rsduck.toml`：

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

启动恢复顺序：

1. 如果 `restore_on_startup = true`，扫描最新正式快照目录。
2. 如果找到快照，执行 `IMPORT DATABASE` 恢复完整库。
3. 如果没有快照，执行 `db.init_sql`。
4. 如果 `init_sql = ""`，启动空内存库。

## 快照

rsduck 使用目录快照保存完整 DuckDB 数据库：

```text
snapshot/
  rsduck_20260703_120000/
    schema.sql
    load.sql
    table_a.parquet
    table_b.parquet
```

保存时先写临时目录：

```text
snapshot/rsduck_yyyyMMdd_HHmmss.tmp
```

成功后重命名为正式目录：

```text
snapshot/rsduck_yyyyMMdd_HHmmss
```

Web 控制台右上角的 `Save Snapshot` 可以手工触发快照。

## 使用案例：K 线实时写入和查询

默认 `init.sql` 会创建 `kline_day` 表：

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

启动 rsduck 后，打开 Web 控制台：

```text
http://127.0.0.1:8080
```

运行压测脚本，持续写入并并发查询：

```powershell
python scripts\rsduck_load_test.py --write-interval 0.5 --write-batch 10 --query-workers 4 --query-interval 0.2
```

在 Web 控制台执行查询：

```sql
SELECT * FROM kline_day ORDER BY bar_time DESC LIMIT 100;
```

也可以查看表信息：

```sql
SELECT schema_name, table_name, estimated_size, column_count
FROM duckdb_tables()
WHERE internal = false
ORDER BY schema_name, table_name;
```

## 自动构建

GitHub Actions workflow 位于：

```text
.github/workflows/ci.yml
```

push 到远程后会执行：

```text
cargo fmt --check
cargo test
cargo build --release
```

并上传 Windows 可执行文件：

```text
target/release/rsduck.exe
```

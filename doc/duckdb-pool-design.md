# rsduck DuckDB 连接池与单写多读架构设计

本文描述 rsduck 当前的 DuckDB 内存库架构。rsduck 在进程内运行一个共享的 in-memory DuckDB，并通过 PostgreSQL wire 协议和 Web SQL 控制台对外提供访问能力。

核心设计目标：

- 使用共享内存 DuckDB，避免多个独立内存库造成数据不一致。
- 使用多个 `try_clone()` DuckDB connection 承担读、写、快照职责。
- 使用 dedicated `std::thread` 执行 DuckDB 同步阻塞 API。
- 使用有界队列提供背压，队列满时返回明确错误。
- 使用 `EXPORT DATABASE` / `IMPORT DATABASE` 目录快照保存和恢复完整库。
- 启动时优先恢复快照；没有快照时执行 `init_sql`；没有 `init_sql` 时启动空库。

## 1. 总体架构

```text
                         rsduck 进程

         PG wire / Web HTTP              timer / manual / shutdown
                |                                  |
                v                                  v
          SQL router / request classifier     snapshot queue
                |                                  |
       +--------+--------------------+             |
       |                             |             |
       v                             v             v
  read dispatcher                write queue   snapshot worker
       |                             |             |
       v                             v             v
  +----+----+----+             single write   snapshot_conn
  |         |    |             worker              |
  v         v    v                |                +-----> snapshot dirs
read_1   read_2 read_3           v                |
  |         |    |             write_conn          |
  |         |    |                |                |
  +---------+----+----------------+----------------+
                         |
                 shared in-memory DuckDB
```

## 2. 连接模型

rsduck 只创建一个 in-memory DuckDB。所有内部连接都来自同一个 base connection：

```rust
let base_conn = duckdb::Connection::open_in_memory()?;
let write_conn = base_conn.try_clone()?;
let read_conn_1 = base_conn.try_clone()?;
let read_conn_2 = base_conn.try_clone()?;
let snapshot_conn = base_conn.try_clone()?;
```

`Connection::open_in_memory()` 每次都会创建独立的内存库，因此不能用多个独立 `open_in_memory()` 组成连接池。`try_clone()` 创建的是指向同一个已打开数据库的新连接，clone 出来的连接共享同一个内存库。

连接角色：

| 连接 | 数量 | 职责 |
|------|------|------|
| `base_conn` | 1 | 创建内存库、启动恢复、执行 `init_sql`，并作为数据库 owner 保留 |
| `write_conn` | 1 | 执行写入、DDL、非查询 SQL |
| `read_conn_N` | 配置化，默认 4 | 执行查询 SQL |
| `snapshot_conn` | 1 | 执行目录快照导出 |

DuckDB 内部仍然通过 MVCC、事务和内部锁处理并发。rsduck 的连接池设计负责移除应用层的全局串行瓶颈，并把读、写、快照隔离到不同 worker。

## 3. Worker 模型

DuckDB Rust API 是同步阻塞 API：

```rust
conn.execute(...);
conn.prepare(...);
rows.next(...);
```

因此，DuckDB 执行层使用固定的 dedicated OS thread：

```text
write worker thread    owns write_conn
read worker thread 1   owns read_conn_1
read worker thread 2   owns read_conn_2
snapshot worker thread owns snapshot_conn
```

Tokio runtime 负责网络服务、连接会话、Web HTTP、定时调度等 async 任务。SQL 执行通过 channel 投递到 DB worker thread，避免 DuckDB 阻塞执行占用 Tokio 网络线程。

## 4. 启动流程

启动顺序：

```text
1. base_conn = Connection::open_in_memory()
2. 如果 restore_on_startup = true，扫描最新正式快照目录
3. 如果找到快照目录，base_conn 执行 IMPORT DATABASE
4. 如果没有快照且 db.init_sql 非空，base_conn 执行 init_sql
5. 如果没有快照且 db.init_sql 为空，启动空内存库
6. write_conn = base_conn.try_clone()
7. read_conn_1..N = base_conn.try_clone()
8. snapshot_conn = base_conn.try_clone()
9. 启动 write worker thread
10. 启动 read worker threads
11. 启动 snapshot worker thread
12. 启动 PG wire server
13. 启动 Web server
14. 启动 snapshot scheduler
```

请求运行期间不会临时创建 DuckDB connection。读、写、快照请求只进入已经预加载好的 worker。

## 5. SQL 路由

### 5.1 查询请求

以下命令走 read pool：

```text
SELECT
WITH
SHOW
DESCRIBE
EXPLAIN
PRAGMA
```

路由方式：

```text
PG/Web 请求 -> read dispatcher -> round-robin 选择 read worker -> 执行 SQL -> 返回结果
```

每个 read worker 独占一个 DuckDB connection。多个 async task 不直接共享同一个 DuckDB connection。

### 5.2 写入和 DDL 请求

除查询命令外，其余 SQL 进入 write queue：

```text
INSERT
UPDATE
DELETE
CREATE
DROP
ALTER
COPY
IMPORT
EXPORT
```

路由方式：

```text
PG/Web 请求 -> bounded write queue -> single write worker -> write_conn
```

写 worker 串行执行写请求，保证写路径顺序可控。

## 6. 队列和背压

rsduck 使用有界队列连接 async 网络层和 blocking DB worker。

当前配置：

```toml
[db]
write_queue_size = 100000
read_queue_size = 1024
snapshot_queue_size = 16
```

队列满时返回明确错误：

```text
write queue is full
read queue is full
snapshot queue is full
```

这种行为可以避免请求无限堆积导致进程内存不可控。

## 7. 查询结果限制

查询结果由 DB worker 收集后返回给 PG/Web 层。当前通过 `max_result_rows` 控制单次查询最大返回行数：

```toml
[db]
max_result_rows = 100000
```

Web 控制台的分页上限与该配置保持一致。大结果集需要分页查询，避免一次性返回过多数据。

## 8. 快照设计

rsduck 使用 DuckDB 数据库目录快照保存完整内存库，不使用单表 parquet 文件作为快照。

保存命令：

```sql
EXPORT DATABASE 'snapshot/rsduck_20260703_120000.tmp' (FORMAT parquet, COMPRESSION zstd);
```

恢复命令：

```sql
IMPORT DATABASE 'snapshot/rsduck_20260703_120000';
```

目录结构：

```text
snapshot/
  rsduck_20260703_120000/
    schema.sql
    load.sql
    table_a.parquet
    table_b.parquet
```

保存流程：

```text
SaveSnapshot
  -> snapshot worker thread
  -> snapshot_conn
  -> EXPORT DATABASE 到 snapshot/rsduck_yyyyMMdd_HHmmss.tmp
  -> rename 到 snapshot/rsduck_yyyyMMdd_HHmmss
```

恢复规则：

- 只扫描 `snapshot.dir` 的直接子目录。
- 合法正式目录名为 `{prefix}_yyyyMMdd_HHmmss`。
- 忽略 `*.tmp` 临时目录。
- 忽略历史残留的 `*.parquet`、`*.parquet.tmp`、`*.tmp.parquet` 文件。
- 多个正式快照目录按时间排序，选择最新目录。
- `IMPORT DATABASE` 失败时启动失败，不自动尝试更早的快照目录。

过期清理只处理合法正式快照目录，不删除临时目录和历史单文件 parquet。

## 9. 初始化 SQL

`init_sql` 是没有快照时的初始化入口。

```toml
[db]
init_sql = "init.sql"
```

规则：

- 成功恢复快照时不执行 `init_sql`。
- 没有快照且 `init_sql` 非空时执行该 SQL 文件。
- 配置了 `init_sql` 但文件不存在时启动失败。
- `init_sql` 执行失败时启动失败。
- `init_sql = ""` 时启动空内存库。
- rsduck 不默认创建业务表。

## 10. PG wire 和 Web 接入

外部客户端可以通过 PG 协议多连接接入：

```text
asyncpg pool / JDBC pool / Navicat
    -> rsduck pg_server
    -> 内部 DuckDB read pool / write queue
```

两层连接池含义不同：

| 层级 | 含义 |
|------|------|
| 外部 PG 连接池 | 客户端到 rsduck 的 TCP 连接池 |
| 内部 DuckDB 连接池 | rsduck 内部多个 `try_clone()` DuckDB connection |

PG session 当前按 statement pooling 处理：每条 SQL 根据命令类型路由到 read pool 或 write queue，执行完成后释放内部 worker。

Web 控制台通过 HTTP 调用 SQL 接口，提供：

- 表列表展示。
- SQL 编辑和执行。
- 查询结果分页。
- 手工保存快照。
- 编辑区和结果区拖动分割。

## 11. 配置

当前主要配置：

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

配置约束：

- `snapshot.prefix` 用于生成和扫描快照目录。
- `snapshot.dir` 是快照根目录，只扫描直接子目录。
- `db.init_sql` 只在没有恢复快照时执行。
- 快照格式固定为 parquet 目录。

## 12. 当前边界

当前设计明确限制以下能力：

- 不支持多个 write worker 同时写同一个内存库。
- 不支持外部替换 `.duckdb` 文件后让连接自动感知。
- 不使用多个独立 `open_in_memory()` 组成连接池。
- 不默认创建 `kline_day` 或其他业务表。
- 不使用单表 `.parquet` 文件作为数据库快照。
- 不在启动恢复失败后自动尝试更早的快照目录。
- 不提供隐式兼容路径或备用执行路径。
- 不允许无限制大结果集。
- 不提供完整 PostgreSQL 跨语句事务语义。

这些边界用于保持执行路径清晰，避免隐藏状态和不可控降级。

## 13. 后续优化方向

可继续增强的方向：

- 结构化写入 API，例如 `append_kline(rows)`，减少高频写入时的大 SQL 拼接成本。
- DuckDB `Appender` 写入路径。
- 查询超时控制。
- PG DataRow 流式返回，减少大结果集二次复制。
- `/status` 和 `/metrics` 监控接口。
- 快照目录大小、最近快照耗时、队列长度、读写耗时等可观测指标。

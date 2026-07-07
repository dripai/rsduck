# rsduck DuckDB 连接池与单写多读架构设计

项目设计总览见 [rsduck-design.md](rsduck-design.md)。本文是执行层深入设计，重点描述 DuckDB 连接、worker、队列、快照和分区调度。

本文描述 rsduck 当前的 DuckDB 内存库架构。rsduck 在进程内运行一个共享的 in-memory DuckDB，并通过 PostgreSQL wire 协议和 Web SQL 控制台对外提供访问能力。

核心设计目标：

- 使用共享内存 DuckDB，避免多个独立内存库造成数据不一致。
- 使用多个 `try_clone()` DuckDB connection 承担读、写、快照职责。
- 使用 dedicated `std::thread` 执行 DuckDB 同步阻塞 API。
- 使用有界队列提供背压，队列满时返回明确错误。
- 使用分区维护调度器生成 managed range partitioned table 的维护任务，但所有 catalog / DuckDB 修改仍统一进入 write queue。
- 使用 `EXPORT DATABASE` / `IMPORT DATABASE` 目录快照保存和恢复完整库。
- 启动时优先恢复快照；没有快照时执行 `init_sql`；没有 `init_sql` 时启动空库。

## 1. 总体架构

```text
                                          rsduck 进程

  PG wire / Web HTTP                 partition timer / write trigger / manual       timer / manual / shutdown
         |                                             |                                      |
         v                                             v                                      v
  SQL router / request classifier              partition scheduler                    snapshot queue
         |                                             |                                      |
         +------------------------------+              v                                      v
         |                              |       maintenance jobs                     snapshot worker
         v                              v              |                                      |
  read dispatcher                  write queue <--------+                                      v
         |                              |                                                snapshot_conn
         v                              v                                                     |
  +------+------+                 single write                                                +-----> snapshot dirs
  |      |      |                 worker                                                      |
  v      v      v                     |                                                        |
read_1 read_2 read_3                  v                                                        |
  |      |      |                  write_conn                                                  |
  |      |      |                     |                                                        |
  +------+------+---------------------+--------------------------------------------------------+
                |
                v
        shared in-memory DuckDB
```

分区维护调度器是 rsduck 进程内的 async task。它只负责扫描 catalog、计算要执行的维护任务，并把任务投递到 `write queue`。它不直接持有 DuckDB connection，也不直接执行 DDL。

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

分区维护调度器不拥有 DuckDB connection。它产生的 `create_range_partition`、`expire_partition`、`refresh_partition_entrypoint` 等任务必须通过 write queue 进入 `write_conn` 执行。

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
partition scheduler    owns no DuckDB connection
```

Tokio runtime 负责网络服务、连接会话、Web HTTP、定时调度等 async 任务。SQL 执行通过 channel 投递到 DB worker thread，避免 DuckDB 阻塞执行占用 Tokio 网络线程。

partition scheduler 可以运行在 Tokio runtime 中。它允许阻塞等待 timer 和 channel 事件，但不得调用 DuckDB Rust API。所有会改变 catalog 或 physical table 的任务都必须转换成 write job，由 single write worker 串行执行。

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
12. 启动 partition scheduler
13. 启动 PG wire server
14. 启动 Web server
15. 启动 snapshot scheduler
```

请求运行期间不会临时创建 DuckDB connection。读、写、快照请求只进入已经预加载好的 worker。

partition scheduler 启动后先等待数据库恢复和 catalog 校验完成。只有 catalog status 为 ready 后，才允许扫描 managed range partitioned table 并投递维护任务。

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

### 5.3 分区维护任务

分区维护任务不是独立执行路径。它们和普通写入、DDL 一样进入 write queue：

```text
partition scheduler
  -> maintenance job
  -> bounded write queue
  -> single write worker
  -> catalog mutation
  -> write_conn
```

维护任务包括：

```text
CreateRangePartition
ExpirePartition
RefreshPartitionEntrypoint
VerifyPartitionManifest
MarkPartitionUnavailable
```

这些任务必须满足：

- 不能绕过 SQL router 的 reserved schema 规则直接接受外部 SQL。
- 不能直接在 scheduler 内执行 DuckDB DDL。
- 必须进入 catalog mutation 流程，写入 journal 和 catalog epoch。
- 必须和用户写入、用户 DDL 保持单写顺序。
- 失败时必须返回明确错误或记录维护告警，不能静默跳过。

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

## 8. 分区维护调度器

rsduck 的 managed range partitioned table 需要自动维护物理分区表、retention 和查询入口。该能力由 partition scheduler 负责发现和调度，但实际修改仍由 single write worker 执行。

### 8.1 设计原则

```text
partition scheduler 只做决策
single write worker 负责执行
write_conn 是唯一修改 DuckDB 和 rsduck_catalog 的连接
```

禁止行为：

- scheduler 直接调用 DuckDB API。
- scheduler 持有 DuckDB connection。
- scheduler 绕过 write queue 修改 `rsduck_catalog.*`。
- scheduler 和用户写入并发执行 DDL。

允许行为：

- scheduler 读取内存中的维护配置。
- scheduler 向 write queue 投递维护 job。
- scheduler 接收 timer、写入触发和 admin/manual 触发。
- scheduler 聚合同一个 parent relation 的重复维护请求。

### 8.2 触发来源

分区维护有三类触发：

| 触发来源 | 用途 | 执行路径 |
|----------|------|----------|
| timer | 定期过期旧分区、校验查询入口、补齐必要维护。 | scheduler -> write queue |
| write trigger | 写入时发现目标分区不存在，创建物理分区。 | write worker 内部生成同事务 mutation |
| manual | admin/operator 手工刷新、修复分区状态。 | SQL/API -> write queue |

timer 触发的维护只负责低频后台巡检，不能成为写入正确性的唯一依赖。写入路径必须能在目标分区缺失时同步创建分区；不可路由数据必须直接写入失败。

### 8.3 维护 Job

维护 job 是 write queue 中的一类内部请求：

| Job | 作用 |
|-----|------|
| `EnsurePartitionedTable` | 校验 parent 分区表和查询入口存在。 |
| `CreateRangePartition` | 创建指定 `partition_value` 的物理分区表。 |
| `ExpirePartition` | 过期并 DROP retention 窗口外的普通分区。 |
| `RefreshPartitionEntrypoint` | 根据 active partitions 重建分区表查询入口。 |
| `VerifyPartitionManifest` | 校验 `rs_partition`、`pg_class` 和 DuckDB physical object 一致。 |
| `MarkPartitionUnavailable` | 将异常分区标记为 unavailable 并记录告警。 |
### 8.4 写入触发创建分区

结构化写入或普通 INSERT 进入 write worker 后，写路径必须先按分区规则路由：

```text
append / insert rows
  -> load parent partition metadata
  -> compute partition_value per row
  -> invalid/null/unroutable rows -> reject write
  -> missing ordinary partitions -> create_range_partition
  -> append rows into physical partitions
  -> refresh entrypoint if new partition was created
  -> update row_count / min_ts / max_ts / checksum
```

该流程必须在 write worker 内串行执行。不能先把数据写入临时表，再让后台 scheduler 异步补分区；否则用户可能查询到不完整数据。

### 8.5 定时维护流程

timer tick 的流程：

```text
1. 如果 catalog status != ready，跳过。
2. 扫描 managed_kind = range_partitioned_table 的 parent relations。
3. 对每个 parent_relid 尝试获取 maintenance lease。
4. 计算 retention 窗口。
5. 生成 ExpirePartition jobs。
6. 生成 VerifyPartitionManifest jobs。
7. 必要时生成 RefreshPartitionEntrypoint jobs。
8. 将 jobs 投递到 write queue。
```

maintenance lease 是进程内互斥标记，用于避免同一个 parent relation 在同一时间被重复维护。lease 不替代 catalog transaction，也不替代 write worker 的串行执行。

### 8.6 Retention 规则

对于：

```sql
WITH (
    partition_unit = 'day',
    retention = '30'
)
```

语义是保留最近 30 个 day partition。scheduler 根据当前时间计算保留窗口，然后对窗口外的 active partitions 执行过期：

```text
active partitions outside retention window
  -> ExpirePartition
```

`hour`、`day`、`month`、`year` 都使用同一条规则：`retention = N` 表示保留最近 N 个 `partition_unit`。

### 8.7 失败和背压

维护 job 进入 write queue 后受同样的背压约束：

- write queue 满时，timer 触发的维护 job 可以丢弃本轮并记录告警，下轮重试。
- write queue 满时，manual 维护请求必须返回明确错误。
- write queue 满时，写入触发的必要分区创建不能静默跳过；写入必须失败。
- 维护 job 失败不得影响其他普通读查询。
- 单个分区表维护失败应标记该 relation 或 partition 为 unavailable，不应导致服务整体不可用。

### 8.8 与 snapshot worker 的关系

partition scheduler 和 snapshot scheduler 是两条不同调度线：

```text
partition scheduler -> write queue -> write_conn
snapshot scheduler  -> snapshot queue -> snapshot_conn
```

snapshot worker 不负责创建、删除或修复分区。snapshot 导出的是当前 DuckDB database 状态，包括 `rsduck_catalog.*`、`rsduck_internal.*` 和分区表查询入口。

如果 snapshot 与分区维护同时发生，DuckDB 的事务和内部一致性负责导出某一时刻的可见状态。rsduck 不在 snapshot worker 中额外执行分区修复。

## 9. 快照设计

rsduck 使用 DuckDB 数据库目录快照保存完整内存库，不使用单表 parquet 文件作为快照。

当前版本的 snapshot 是全量目录快照，不是增量快照。每次保存都重新导出当前 DuckDB database 的完整可恢复状态。

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

## 10. 初始化 SQL

`init_sql` 是没有快照时的初始化入口。

```toml
log_level = "info"

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

## 11. PG wire 和 Web 接入

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

## 12. 配置

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

配置约束：

- `snapshot.prefix` 用于生成和扫描快照目录。
- `snapshot.dir` 是快照根目录，只扫描直接子目录。
- `db.init_sql` 只在没有恢复快照时执行。
- 快照格式固定为 parquet 目录。
- `partition.maintenance_enabled` 控制 timer 触发的后台分区维护。写入触发的必要分区创建不受该开关影响。
- `partition.maintenance_interval_secs` 控制 retention 和 refresh 类维护扫描间隔。
- `partition.verify_interval_secs` 控制 manifest 校验扫描间隔。
- `partition.max_jobs_per_tick` 限制单次 tick 投递到 write queue 的维护 job 数量。

## 13. 当前边界

当前设计明确限制以下能力：

- 不支持多个 write worker 同时写同一个内存库。
- 不支持外部替换 `.duckdb` 文件后让连接自动感知。
- 不使用多个独立 `open_in_memory()` 组成连接池。
- 不默认创建 `kline_day` 或其他业务表。
- 不使用单表 `.parquet` 文件作为数据库快照。
- 不把当前 snapshot 称为增量快照；当前 snapshot 只表示完整 DuckDB database 目录导出。
- 不在启动恢复失败后自动尝试更早的快照目录。
- 不提供隐式兼容路径或备用执行路径。
- 不允许无限制大结果集。
- 不提供完整 PostgreSQL 跨语句事务语义。
- 不允许 partition scheduler 直接持有或调用 DuckDB connection。
- 不允许后台分区维护绕过 write queue。
- 不允许因为 timer 维护失败而静默改变分区表查询入口。

这些边界用于保持执行路径清晰，避免隐藏状态和不可控降级。

## 14. 后续优化方向

可继续增强的方向：

- 结构化写入 API，例如 `append_kline(rows)`，减少高频写入时的大 SQL 拼接成本。
- DuckDB `Appender` 写入路径。
- 查询超时控制。
- PG DataRow 流式返回，减少大结果集二次复制。
- `/status` 和 `/metrics` 监控接口。
- 快照目录大小、最近快照耗时、队列长度、读写耗时等可观测指标。
- partition maintenance 指标，包括 pending jobs、last tick、failed jobs、unavailable partitions。
- 未来增量恢复能力。目标形态应是 `base snapshot + catalog journal + WAL/redo segments`：启动时先恢复最近一次全量目录快照，再按已确认的日志位置顺序重放后续增量。该能力需要先定义日志格式、LSN/epoch 边界、校验、幂等重放、截断策略和损坏处理规则；当前版本不实现，也不把现有 snapshot 命名为增量快照。

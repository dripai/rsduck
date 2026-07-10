# rsduck 总体架构设计

本文从架构视角说明 rsduck 的整体设计：进程如何启动，内存 DuckDB 如何被包装成服务，线程和 worker 如何协作，以及 Web/MySQL 两类外部接口如何进入同一套执行引擎。

## 1. 设计目标

rsduck 的目标不是把 DuckDB 原封不动暴露给外部客户端，而是把 DuckDB 包装成一个受控的内存数据库服务：

- 对外提供 MySQL wire 协议，方便 Navicat 和 MySQL 客户端连接。
- 对外提供 Web SQL 控制台，方便查询、快照和 Parquet 表导入。
- 使用 `rsduck_catalog.rs_*` 管理对象、权限、依赖、快照元数据。
- 所有可恢复状态进入 Snapshot v2。
- 不支持的能力返回明确错误，不隐式回退到 DuckDB 内部 catalog 或历史路径。

整体架构可以概括为：

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

## 2. 启动流程

主入口在 `src/main.rs`。服务启动时按固定顺序执行：

1. 读取 `rsduck.toml`，初始化日志。
2. 校验 snapshot prefix，避免生成危险快照目录名。
3. 获取 `.rsduck.lock` 进程锁，阻止同一个工作目录启动多个实例。
4. 如果开启 `snapshot.restore_on_startup`，查找最新 Snapshot v2。
5. 调用 `DbHandle::open` 创建内存 DuckDB，并执行恢复或初始化。
6. 根据配置启动分区维护任务。
7. 启动 MySQL wire 服务。
8. 启动周期性 snapshot 任务。
9. 如果开启 Web，启动 Axum Web 服务；否则等待关闭信号。
10. 收到关闭信号时保存一次关闭快照，然后停止 worker。

关键点是：工作目录是运行时状态的边界。`rsduck.toml`、`init.sql`、`snapshot/`、`logs/`、`.rsduck.lock` 都以当前工作目录为基准。Windows 服务部署时必须明确 working directory，否则会出现读错配置、找不到快照或锁文件漂移的问题。

## 3. 初始化和恢复

`DbHandle::open` 创建一个 in-memory DuckDB 连接作为基础连接，然后调用恢复/初始化逻辑：

- 如果传入 snapshot 目录，从 Snapshot v2 恢复 catalog、schema、普通表数据、索引、视图、macro/函数。
- 如果没有可用 snapshot，则创建全新的 `rsduck_catalog`。
- 新库存在 `init.sql` 时，执行初始化 SQL。

`init.sql` 是内部初始化入口，可以包含多条语句；但每条 DDL 仍应通过 catalog-aware mutation，不应绕过 catalog 直接创建业务对象。

恢复顺序的核心原则是先恢复事实来源，再恢复物理对象：

```text
catalog.duckdb
    -> schema
    -> ordinary table data
    -> indexes
    -> views
    -> macros/functions
    -> checksum and consistency checks
```

如果 manifest、catalog checksum、格式版本或视图/macro checksum 不一致，恢复直接失败。业务数据文件缺失时，对应 relation 标记为 unavailable，避免把不完整对象伪装成正常表。

## 4. DuckDB 连接模型

rsduck 使用一个基础 in-memory DuckDB 实例，并通过 `Connection::try_clone()` 创建多个同源连接：

- 一个 write connection。
- N 个 read connection。
- 一个 snapshot connection。
- 一个保留在 `DbEngine` 中的 base connection。

这些连接指向同一个内存数据库实例，但每个连接仍有自己的连接级状态。因此：

- 外部请求不应依赖临时表跨请求存在。
- 外部显式事务不应跨 Web/MySQL 请求使用。
- 同一个用户的两次查询不保证落到同一个 read worker。
- 程序内部如果确实需要临时表，应设计固定在一个 worker/connection 内完成的复合任务。

## 5. Worker 和线程服务

rsduck 在 tokio 异步服务外层接入网络请求，但 DuckDB 执行放在线程 worker 中完成。这样可以避免阻塞 tokio runtime，同时保留 DuckDB connection 的同步调用模型。

### 5.1 Write Worker

write worker 是单线程串行执行入口，负责：

- DDL。
- DML。
- `COPY FROM`。
- 用户、角色、权限管理。
- Parquet 导入。
- Web/MySQL 认证查询。

写操作进入 `write_tx` 有界队列。队列满时返回 queue full 错误，不切换到其他路径执行。这个设计让写入顺序清晰，也让 catalog mutation 的原子性更容易维护。

### 5.2 Read Worker Pool

read worker pool 有 `db.read_workers` 个线程。只读查询按 round-robin 分配：

```text
next_read % read_workers
```

典型进入读 worker 的语句包括：

- `SELECT`
- `WITH`
- `EXPLAIN`
- `SHOW ...`
- `DESCRIBE`
- `COPY ... TO`

读 worker 不持有 snapshot/write gate。它们服务于普通查询吞吐，但不承担 catalog mutation。

### 5.3 Snapshot Worker

snapshot worker 使用独立 connection 和独立队列，负责保存 Snapshot v2。

snapshot worker 与 write worker 共用一个 `snapshot_write_gate`：

```text
write worker ----+
                 +---- snapshot_write_gate
snapshot worker -+
```

写入和快照不能同时进行。这样可以保证 snapshot 导出的 catalog 和业务数据来自同一个稳定状态。读查询可以继续走 read worker，但对外一致性边界以写入和快照的串行为准。

### 5.4 Partition Maintenance Task

如果开启 `partition.maintenance_enabled`，主进程会启动周期性分区维护任务。它调用：

```sql
CALL rsduck_run_partition_maintenance()
```

该调用通过 write route 执行，因为维护可能创建、标记或清理受管分区。

## 6. 内部命令模型

网络层不直接拿 DuckDB connection。所有操作都通过 `DbHandle` 封装为命令：

```text
execute_typed_sql_as
execute_typed_sql_with_params_as
describe_sql_with_params_as
save_snapshot_as
import_parquet_tables_as
authenticate
run_partition_maintenance
```

这些 API 再被翻译成 worker 命令：

```text
SqlCommand::RunTyped
SqlCommand::Describe
SqlCommand::Authenticate
SqlCommand::ImportParquet
SqlCommand::Shutdown

SnapshotCommand::Save
SnapshotCommand::Shutdown
```

命令通过有界 channel 发送，结果通过 oneshot channel 返回。worker 内部使用 `catch_unwind` 包住执行逻辑，避免 DuckDB 或业务代码 panic 直接杀掉调用方任务。

## 7. SQL 路由

外部 SQL 进入执行层前先走 `route_sql`：

1. 使用 DuckDB dialect 的 sqlparser 解析 SQL。
2. 要求一次请求只能有一条 statement。
3. 根据 statement 类型判断 read/write route。
4. 生成命令名，用于执行结果和鉴权。

读写分类的基本规则：

```text
SELECT / WITH / SHOW / DESCRIBE / EXPLAIN / COPY TO -> read
INSERT / UPDATE / DELETE / DDL / GRANT / REVOKE / CALL / COPY FROM -> write
```

拒绝多语句是产品约束，不是 DuckDB 原生限制。它简化了：

- 路由判定。
- 权限校验。
- Web 单结果集响应。
- MySQL protocol 当前实现。
- 临时对象和事务边界。

## 8. Web 上游链路

Web 服务由 Axum 提供，主要端点包括：

```text
GET  /                  Web 页面
POST /login             登录
POST /logout            退出
GET  /session           当前会话
POST /sql               SQL 查询/执行
POST /snapshot          手工快照
GET  /parquet-import         获取 Parquet 导入根目录
POST /parquet-import         执行 Parquet 表导入
```

Web 链路如下：

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

Web session 使用 HttpOnly、SameSite=Lax cookie。`POST /sql` 会对没有顶层 `LIMIT/OFFSET` 的 `SELECT/WITH` 自动包装分页：

```sql
SELECT * FROM (<user_sql>) __rsduck_page LIMIT <page_size> OFFSET <offset>
```

显式带分页的 SQL 保持原样。非查询语句不做分页包装。

## 9. MySQL 上游链路

MySQL 服务监听 TCP，处理 MySQL handshake、认证和命令循环。主要支持：

- `COM_QUERY`
- `COM_STMT_PREPARE`
- `COM_STMT_EXECUTE`
- `COM_STMT_CLOSE`
- `COM_STMT_RESET`
- `COM_PING`
- `COM_INIT_DB`

MySQL 链路如下：

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

MySQL session 维护当前 database、用户名和 prepared statement。`COM_INIT_DB` 改变 session database；空 database 或 `memory` 会映射到 `main`。

## 10. Snapshot 链路

Snapshot 有三种触发方式：

- 周期性任务。
- Web 手工保存。
- 正常关闭前保存。

保存过程：

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

Web 手工保存会携带 username，并执行 `authorize_snapshot`。系统周期保存和关闭保存使用 system 身份。

## 11. Parquet 导入链路

Parquet 导入只从 Web 入口提供，不开放为普通外部 SQL：

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

导入规则：

- 单个 Parquet 文件对应一张普通表。
- 目录模式导入顶层所有 `.parquet` 文件。
- 目录模式是一文件一表，不自动 union。
- 批量导入原子执行，任一文件失败则整批回滚。
- 所有导入表进入 `rsduck_catalog`，可被权限、快照和 Navicat metadata 管理。

## 12. 下游存储边界

rsduck 的下游只有内存 DuckDB 和 snapshot 文件。它不依赖外部数据库系统表，也不把 MySQL metadata 当作事实来源。

运行时状态：

```text
in-memory DuckDB
  -> business schemas and objects
  -> rsduck_catalog.rs_*
  -> rsduck_internal physical partitions
```

持久化状态：

```text
snapshot/
  -> catalog.duckdb
  -> data/*.parquet
  -> manifest.json
```

日志和锁：

```text
logs/
.rsduck.lock
.rsduck.lock.guard
```

## 13. 关闭流程

收到关闭信号后：

1. Web graceful shutdown 开始。
2. 保存关闭快照。
3. abort 周期 snapshot task。
4. abort 分区维护 task。
5. abort MySQL task。
6. 向 write/read/snapshot worker 发送 shutdown 命令。
7. join worker 线程。
8. 释放进程锁。

强制结束进程可能跳过关闭快照，也可能留下锁文件。恢复时应先确认 PID 是否仍存在。

## 14. 架构约束

后续开发必须保持以下约束：

- 外部入口一次只执行一条 SQL。
- 外部不支持跨请求事务和临时表复用。
- 所有 DDL 必须走 catalog-aware mutation。
- 所有写入和 snapshot 必须串行。
- MySQL/Web 只是入口差异，不应形成两套执行语义。
- 不为缺失能力增加静默 fallback。
- 所有可恢复状态必须进入 Snapshot v2。

如果要突破这些约束，应先修改产品契约、文档和测试，再改代码。

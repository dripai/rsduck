# rsduck DuckDB 连接池与单写多读设计

本文记录 rsduck 下一版长期稳定架构。目标不是泛泛地“提升并发”，而是把当前 `Mutex<Connection>` 串行模型升级为可控的内部 DuckDB 连接池。

## 1. 关键结论

### 1.1 `Mutex<Connection>` 不是长期方案

当前实现是：

```text
PG/Web 请求
  -> db::execute_sql
  -> spawn_blocking
  -> Mutex<DuckDB Connection>
  -> 单个内存 DuckDB 连接
```

这个模型简单，但所有读、写、快照都抢同一把锁。慢查询、大批写入、快照都会互相阻塞。`Mutex` 只产生排队效果，不是真正可控队列，不能自然支持批量写、优先级、背压、队列长度统计、graceful shutdown drain。

### 1.2 共享内存库要用 `try_clone()`

不要用多个独立的 `Connection::open_in_memory()` 来做连接池。它们是不同的内存数据库，数据不共享。

正确方式是先创建一个 base connection，再从它克隆连接：

```rust
let base_conn = duckdb::Connection::open_in_memory()?;
let write_conn = base_conn.try_clone()?;
let read_conn_1 = base_conn.try_clone()?;
let read_conn_2 = base_conn.try_clone()?;
let read_conn_3 = base_conn.try_clone()?;
let snapshot_conn = base_conn.try_clone()?;
```

`try_clone()` 的语义是“创建一个连接到已经打开的数据库的新连接”。本地 duckdb-rs 源码和测试都验证了：clone 出来的连接能看到原连接的表和写入。

### 1.3 DuckDB 内部仍有并发控制

改成多连接后，不等于完全无锁。它只是去掉 rsduck 外层那把全局 `Mutex`，让 DuckDB 内部用 MVCC、事务和内部锁做并发控制。

预期效果：

- 多个查询可以分配到不同 read connection 并发执行。
- 高频 append 写入由单写 worker 批量写，减少 SQL 批次数。
- 读和写不再因为 rsduck 外层 Mutex 完全串行。
- DDL、快照、checkpoint、大事务、冲突更新仍可能产生内部等待。

### 1.4 DB worker 使用 dedicated `std::thread`

PG wire 和 Web server 仍然运行在 Tokio async runtime 上；DuckDB 执行层不要直接放在 `tokio::spawn` 里。

DuckDB Rust API 是同步阻塞 API：

```rust
conn.execute(...);
conn.prepare(...);
rows.next(...);
```

长期方案中，每个 DB worker 使用一个 dedicated OS thread：

```text
write worker thread    owns write_conn
read worker thread 1   owns read_conn_1
read worker thread 2   owns read_conn_2
snapshot worker thread owns snapshot_conn
```

请求通过 channel 从 Tokio async 世界投递到 DB worker thread。这样 DuckDB 阻塞执行不会占住 Tokio 网络线程，也不需要 `Mutex<Connection>`。

`tokio::spawn` 只用于网络服务、连接会话、定时调度等 async 任务；DB SQL 执行由 `std::thread::spawn` 创建的固定 worker thread 处理。

## 2. 目标架构

```text
                         rsduck 进程

         PG wire / Web HTTP
                |
                v
          SQL router / request classifier
                |
       +--------+--------------------+
       |                             |
       v                             v
  read dispatcher                write queue
       |                             |
       v                             v
  +----+----+----+             single write worker
  |         |    |                    |
  v         v    v                    v
read_1   read_2 read_3            write_conn
  |         |    |                    |
  +---------+----+--------------------+
                |
        shared in-memory DuckDB
                |
           snapshot_conn
                |
          parquet snapshots
```

### 2.1 连接角色

| 连接 | 数量 | 作用 |
|------|------|------|
| `base_conn` | 1 | 启动时创建内存库、恢复快照、初始化 schema；保留为 owner |
| `write_conn` | 1 | 只做写入和写相关 DDL |
| `read_conn_N` | 配置化，默认 4 | 只做查询 |
| `snapshot_conn` | 1 | 做定时快照、手工快照、退出前快照 |

所有连接都从 `base_conn.try_clone()` 创建，必须共享同一个 in-memory database handle。

## 3. 启动流程

推荐启动顺序：

```text
1. base_conn = Connection::open_in_memory()
2. 如果配置 restore_on_startup = true，base_conn 从最新 parquet 恢复
3. 如果没有快照，base_conn 创建 schema
4. write_conn = base_conn.try_clone()
5. 创建 read_conn_1..N = base_conn.try_clone()
6. snapshot_conn = base_conn.try_clone()
7. 使用 `std::thread::spawn` 启动 write worker thread
8. 使用 `std::thread::spawn` 启动 read worker threads / read pool
9. 使用 `std::thread::spawn` 启动 snapshot worker thread
10. 使用 `tokio::spawn` 或主 async task 启动 PG wire server
11. 使用主 async task 启动 Web server
12. 使用 Tokio task 启动 snapshot scheduler，scheduler 只发命令，不直接执行 DuckDB
```

不要在运行时为每个请求临时 `open_in_memory()`。请求路径只能投递命令到预加载好的 DB worker thread。

## 4. 请求路由规则

### 4.1 读请求

下列命令走 read pool：

```text
SELECT
WITH
SHOW
DESCRIBE
EXPLAIN
PRAGMA 查询类
```

实现方式：

```text
PG/Web 请求 -> read dispatcher -> 选择一个 read worker thread -> 执行 SQL -> 返回结果
```

初版可以做 round-robin：

```rust
let idx = next.fetch_add(1, Ordering::Relaxed) % read_workers.len();
```

每个 read worker thread 独占一个 `Connection`。不要多个 async task 直接共享同一个 connection。

### 4.2 写请求

下列命令走 write queue：

```text
INSERT
UPDATE
DELETE
CREATE
DROP
ALTER
COPY FROM
COPY TO 写表场景
```

实现方式：

```text
PG/Web 写请求 -> bounded channel -> single write worker thread -> write_conn
```

写 worker thread 持有 `write_conn`，按顺序处理写命令。

### 4.3 不支持的混合 SQL

第一版不要支持多语句混合事务和复杂兼容模式，例如：

```sql
INSERT INTO t VALUES (...); SELECT * FROM t;
```

遇到多语句可以返回明确错误：

```text
multi-statement SQL is not supported by pooled mode
```

不要隐式拆分 SQL，也不要自动 fallback 到单连接路径。

## 5. 写入队列设计

### 5.1 命令结构

建议定义：

```rust
enum DbCommand {
    Query {
        sql: String,
        resp: oneshot::Sender<Result<SqlResult, String>>,
    },
    Execute {
        sql: String,
        resp: oneshot::Sender<Result<SqlResult, String>>,
    },
    AppendKline {
        rows: Vec<KlineRow>,
        resp: oneshot::Sender<Result<usize, String>>,
    },
    SaveSnapshot {
        dir: String,
        resp: oneshot::Sender<Result<String, String>>,
    },
    Shutdown {
        resp: oneshot::Sender<()>,
    },
}
```

`AppendKline` 是长期高频写入的关键，不要一直拼通用 SQL。初期可以继续接受 SQL 写入，后续把行情写入接口改成结构化数据。

`resp` 可以使用 `tokio::sync::oneshot::Sender`，因为请求发起方在 Tokio async 世界里等待结果；但 DuckDB worker 自身是 `std::thread`，只负责在执行完成后调用 `resp.send(result)`。

### 5.2 有界队列

写队列必须有上限：

```toml
[db]
write_queue_size = 100000
```

DB worker 是 dedicated `std::thread`，因此写队列推荐使用线程 channel。长期方案建议引入 `crossbeam-channel`：

```rust
let (write_tx, write_rx) = crossbeam_channel::bounded(write_queue_size);
```

也可以使用 `std::sync::mpsc`，但标准库 channel 没有内建 bounded queue，背压实现更麻烦。不要让 DuckDB 执行层依赖 `tokio::spawn` 直接跑阻塞 SQL。

队列满时返回明确错误：

```text
write queue is full
```

不要无限堆内存。

### 5.3 批量写入

写 worker 应该合并小写入：

```toml
[db]
write_batch_rows = 1000
write_flush_ms = 50
```

触发条件：

```text
累计 rows >= write_batch_rows
或距离上次 flush >= write_flush_ms
```

长期建议使用 DuckDB `Appender` 写 `kline_day`，优先级高于拼接大 `INSERT VALUES` 字符串。

## 6. 读连接池设计

### 6.1 初版简单读池

```rust
struct ReadPool {
    workers: Vec<std::sync::mpsc::Sender<ReadCommand>>,
    next: AtomicUsize,
}
```

每个 read worker：

```rust
struct ReadWorker {
    conn: duckdb::Connection,
    rx: std::sync::mpsc::Receiver<ReadCommand>,
}
```

查询请求发给某一个 worker thread，worker thread 在自己的固定 OS 线程里执行 DuckDB 查询并返回结果。

### 6.2 查询限制

必须配置限制：

```toml
[db]
read_workers = 4
max_result_rows = 100000
query_timeout_ms = 30000
```

Web 页面分页上限可以和 `max_result_rows` 一致。大查询不允许无限返回。

### 6.3 结果流式化

当前代码会把查询结果收集成 `Vec<Vec<String>>`。长期应改为分批编码 PG `DataRow`，避免大结果集二次复制。

优先级：

1. 先引入 read pool。
2. 再做 max rows / timeout。
3. 最后做 PG row 流式返回。

## 7. 快照设计

你的业务允许快照延迟，所以快照不要参与实时读写路径。

### 7.1 快照连接

使用 `snapshot_conn = base_conn.try_clone()`。

定时、手工、退出前保存都调用同一条路径：

```text
SaveSnapshot -> snapshot_conn -> COPY kline_day TO parquet
```

### 7.2 快照频率

快照不需要紧贴写入。建议：

```toml
[snapshot]
interval_secs = 900
retain_hours = 2
```

如果需要更强恢复能力，可以把 write worker 的结构化写入追加到 WAL-like 文本/二进制日志，快照仍然低频。

### 7.3 原子写入

快照文件要先写临时文件：

```text
kline_day_20260702_101500.tmp.parquet
```

成功后 rename：

```text
kline_day_20260702_101500.parquet
```

恢复时只扫描正式 `.parquet`，不要扫描 `.tmp.parquet`。

## 8. PG wire 集成方式

外部 PG 客户端可以继续多连接接入：

```text
asyncpg pool / JDBC pool / Navicat 多连接
    -> rsduck pg_server
    -> 内部 DuckDB read pool / write queue
```

这两层连接池不同：

| 层级 | 含义 |
|------|------|
| 外部 PG 连接池 | 客户端到 rsduck 的 TCP 连接池 |
| 内部 DuckDB 连接池 | rsduck 内部多个 `try_clone()` DuckDB connection |

PG session 不应直接绑定 DuckDB connection。第一版采用 statement pooling：

```text
每条 SQL 根据类型路由到 read pool 或 write queue
执行完成后释放内部 worker
```

暂时限制：

- 不支持跨多条 SQL 的事务语义。
- 不支持 session 级临时表。
- 不支持依赖同一 DuckDB connection 状态的 prepared statement。
- Extended Query 可先返回明确错误，或只支持无参数查询。

如果未来要兼容事务，再做 transaction pooling：

```text
BEGIN -> 绑定一个内部连接
COMMIT/ROLLBACK -> 释放内部连接
```

## 9. 配置建议

新增配置：

```toml
[db]
mode = "memory"
read_workers = 4
write_queue_size = 100000
write_batch_rows = 1000
write_flush_ms = 50
max_result_rows = 100000
query_timeout_ms = 30000

[snapshot]
restore_on_startup = true
dir = "snapshot"
interval_secs = 900
retain_hours = 2
```

第一版不要同时提供多种兼容模式。建议直接实现 `mode = "memory"` + `try_clone()` 共享内存库。

## 10. 迁移步骤

### Step 1：抽象 DbEngine

替换当前全局：

```rust
static DB_INSTANCE: OnceLock<Mutex<Connection>>
```

改为：

```rust
static DB_ENGINE: OnceLock<DbEngineHandle>
```

`DbEngineHandle` 提供：

```rust
async fn query(sql: String) -> Result<SqlResult, String>
async fn execute(sql: String) -> Result<SqlResult, String>
async fn save_snapshot(dir: String) -> Result<String, String>
```

### Step 2：启动时创建 base_conn 和 cloned connections

```rust
let base = Connection::open_in_memory()?;
restore_or_create_schema(&base)?;

let write_conn = base.try_clone()?;
let read_conns = (0..read_workers)
    .map(|_| base.try_clone())
    .collect::<Result<Vec<_>>>()?;
let snapshot_conn = base.try_clone()?;
```

保留 `base` 在 `DbEngine` 内，不要 drop 到难以理解的生命周期里。

### Step 3：读请求进入 read pool

先实现简单 round-robin worker，不要上来做复杂调度。

### Step 4：写请求进入 single write worker

先保证写请求串行可控，再做批量合并。

### Step 5：结构化行情写入

新增内部 API：

```rust
append_kline(rows: Vec<KlineRow>)
```

Web/PG 普通 SQL 仍保留，但高频脚本和后续行情接入走结构化写入。

### Step 6：快照改为 snapshot_conn

手工快照、定时快照、退出快照统一发 `SaveSnapshot` 命令。

## 11. 不做的事情

第一版不要做这些：

- 不做多个 write worker 同时写同一张表。
- 不做外部替换 `.duckdb` 文件后让连接自动感知。
- 不做多个独立 `open_in_memory()` 拼成连接池。
- 不做隐式 fallback 到旧 `Mutex<Connection>` 路径。
- 不做无限制大查询。
- 不做完整 PostgreSQL 事务兼容。

这些都会把边界搞模糊，影响稳定性。

## 12. 最终目标

最终目标是：

```text
共享内存 DuckDB
+ try_clone 预加载连接池
+ 单写 worker 批量写
+ 多读 worker 并发查
+ 快照异步低频保存
+ PG wire / Web 统一路由
+ 有界队列和明确错误
```

这套方案仍然保持 rsduck 的核心定位：内存数据库、PG 协议入口、快照恢复；同时去掉当前最主要的瓶颈：所有 SQL 抢同一个 `Mutex<Connection>`。

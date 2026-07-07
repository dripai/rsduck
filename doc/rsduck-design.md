# rsduck 项目设计文档

本文面向两类读者：

- 数据库使用者：关心 rsduck 能提供什么能力、如何连接、如何建表和查询、哪些行为可靠、哪些边界不能依赖。
- 开发者：关心代码模块如何划分、SQL 如何进入执行链路、catalog 如何维护、分区和快照如何与 DuckDB 交互。

本文是项目设计总览。更细的执行层设计见 [duckdb-pool-design.md](duckdb-pool-design.md)，更细的 catalog 设计见 [rsduck_pg_catalog_design.md](rsduck_pg_catalog_design.md)。

## 1. 项目定位

rsduck 是一个基于 DuckDB 的内存数据库中间件服务。它在进程内维护一个共享的 in-memory DuckDB，对外提供 PostgreSQL wire 协议、Web SQL 控制台和 HTTP SQL API，并通过目录快照实现恢复。

rsduck 不尝试复刻 PostgreSQL 内核。DuckDB 是唯一 SQL 执行引擎；rsduck 在 DuckDB 外层提供：

- PG-compatible 元数据 catalog。
- 账号、角色和权限控制。
- 单写多读执行调度。
- managed range partitioned table。
- 目录快照和启动恢复校验。

核心原则：

- 内存库只有一个，所有内部连接都来自同一个 DuckDB base connection 的 `try_clone()`。
- 写入、DDL、catalog mutation 必须走 single write worker。
- `rsduck_catalog.*` 是元数据事实来源。
- `pg_catalog.*` 和 `information_schema.*` 是只读兼容投影。
- 不支持的兼容行为必须返回明确错误或明确空结果，不做隐式 fallback。

## 2. 给数据库使用者看的设计

### 2.1 访问入口

rsduck 默认提供两个主要访问入口：

| 入口 | 默认地址 | 用途 |
|------|----------|------|
| Web SQL Console | `http://127.0.0.1:8080` | 浏览表、执行 SQL、分页查看结果、手工保存快照 |
| PostgreSQL wire | `127.0.0.1:15432` | 使用 psql、DBeaver、Navicat、ORM 或 PG 驱动连接 |

Web 和 PG wire 都使用 catalog 用户认证。首次 bootstrap 会创建默认管理员：

```text
username = admin
password = admin
role     = admin
```

rsduck 不强制首次登录修改密码。生产部署时管理员应主动修改默认密码。

#### 2.1.1 正常修改密码

管理员仍可登录时，直接通过 Web SQL Console 或 PG wire 执行：

```sql
ALTER USER admin PASSWORD 'new_password';
```

该语句必须走 single write worker 和 catalog mutation 流程，更新 `rsduck_catalog.rs_user.password_hash`，写入 catalog journal，推进 catalog epoch，并刷新 catalog checksum。禁止直接写 `rsduck_catalog.rs_user`。

#### 2.1.2 忘记 admin 密码时的离线重置

如果忘记 `admin` 密码，且没有其他 active admin 用户，不能通过在线 SQL 重置。目标维护方式是提供离线命令：

```powershell
rsduck reset-admin-password
rsduck reset-admin-password --password admin123
```

不传 `--password` 时固定把内置 `admin` 用户密码重置为 `admin`；传入 `--password` 时重置为指定密码。命令不启动 PG wire、不启动 Web、不启动 read/write/snapshot worker，只在当前进程内创建一个临时 DuckDB connection 处理最新 snapshot。

离线重置流程必须满足：

1. 先停止正在运行的 rsduck 服务。
2. 命令启动后尝试获取 rsduck 进程独占锁，并读取或写入 `.rsduck.lock`。
3. 如果锁被占用，说明 rsduck 仍在运行，命令必须失败并提示锁文件中的 PID。
4. 如果锁文件存在但可独占获取，视为 stale lock，可读取其中 PID 用于提示，然后覆盖。
5. 读取 `rsduck.toml`，找到 `snapshot.dir` 下最新正式 snapshot。
6. 打开临时 DuckDB connection，`IMPORT DATABASE` 最新 snapshot。
7. 校验 snapshot manifest 和 catalog checksum。
8. 执行受控 catalog mutation：`ALTER USER admin PASSWORD '<new_password>'`。
9. `EXPORT DATABASE` 到新的 `.tmp` snapshot 目录。
10. 写入新的 `rsduck_snapshot_manifest.json`。
11. 原子 rename 为新的正式 snapshot 目录，不覆盖原 snapshot。

`.rsduck.lock` 保存 JSON 诊断信息，至少包含：

```json
{
  "pid": 12345,
  "mode": "service",
  "started_at": "2026-07-07T21:40:00+08:00",
  "workdir": "D:\\workspace\\12.aiwork\\demo\\rsduck",
  "pg_bind": "127.0.0.1:15432",
  "web_bind": "127.0.0.1:8080"
}
```

rsduck 当前是单进程服务，PG wire、Web、read/write worker、snapshot worker 和 partition maintenance 都在同一个 OS 进程内，因此 lock 中只记录一个 PID。跨进程独占锁可由 `.rsduck.lock.guard` 持有，`.rsduck.lock` 本身用于在锁被占用时读取 PID 和启动参数。

判断服务是否停止必须以文件独占锁为准，PID 只用于诊断提示。禁止直接修改 snapshot 中的 parquet 文件，因为需要同步 catalog journal、catalog epoch、catalog checksum 和 snapshot manifest。

### 2.2 对象模型

普通业务对象默认创建在 DuckDB 默认 schema `main` 下。rsduck 保留以下 schema：

| schema | 对使用者的含义 |
|--------|----------------|
| `main` | 默认业务 schema |
| `pg_catalog` | PG 客户端元数据兼容查询入口，只读 |
| `information_schema` | SQL 标准元数据兼容查询入口，只读 |
| `rsduck_catalog` | rsduck 内部元数据事实表，普通用户不能直接写入 |
| `rsduck_internal` | rsduck 内部物理分区表等生成对象，普通用户不能直接依赖 |

使用者应查询稳定的业务表名，不应直接查询 `rsduck_internal.*` 的物理分区表。

### 2.3 SQL 执行语义

rsduck 会按 SQL 类型选择执行路径：

- `SELECT`、`SHOW`、`DESCRIBE` 等查询进入 read worker。
- `INSERT`、`UPDATE`、`DELETE`、DDL、账号权限变更进入 single write worker。
- `pg_catalog.*` / `information_schema.*` 查询会改写为 rsduck catalog projection。
- 对 reserved schema 的直接写入会被拒绝。
- 单次 SQL 请求只支持一条语句。

查询结果会受 `db.max_result_rows` 限制。Web 控制台会在只读查询外层增加分页包装。

### 2.4 分区表

rsduck 支持 managed range partitioned table。用户看到的是稳定的分区表名，rsduck 在内部维护多个物理分区表，并通过 entrypoint view 提供统一查询入口。

建表示例：

```sql
CREATE TABLE ods_access_log (
    id BIGINT,
    user_id VARCHAR(64),
    access_time TIMESTAMP NOT NULL,
    content TEXT
)
PARTITION BY RANGE (access_time)
WITH (
    partition_unit = 'day',
    retention = '30'
);
```

规则：

- `PARTITION BY RANGE` 只允许一个字段。
- 分区字段必须是 `DATE` 或 `TIMESTAMP`。
- `partition_unit` 只允许 `hour`、`day`、`month`、`year`。
- `TIMESTAMP` 字段支持 `hour/day/month/year`。
- `DATE` 字段不允许 `hour`，只能使用 `day/month/year`。
- `retention = '30'` 表示保留最近 30 个 `partition_unit` 时间窗口。
- 空值或无法路由的脏数据进入 null partition，仍可通过分区表查询到。
- retention 自动清理不会删除 null partition。

查询分区：

```sql
SHOW PARTITIONS ods_access_log;
```

业务查询仍然查逻辑表：

```sql
SELECT *
FROM ods_access_log
WHERE access_time >= TIMESTAMP '2026-07-01 00:00:00';
```

### 2.5 快照与恢复

rsduck 使用 DuckDB `EXPORT DATABASE` / `IMPORT DATABASE` 做目录快照。快照包含：

- 业务表、视图、索引。
- `rsduck_catalog.*` 内部元数据。
- `rsduck_internal.*` 内部物理分区表。

启动顺序：

```text
1. 打开新的 in-memory DuckDB。
2. 如果配置允许恢复，导入最新正式快照。
3. 没有快照时执行 init_sql。
4. 没有 init_sql 时启动空库。
5. bootstrap 或加载 rsduck catalog。
6. 校验 catalog、journal、checksum、物理对象和分区 entrypoint。
7. 启动 PG wire 和 Web 服务。
```

如果单张表、视图、索引或单个分区物理对象异常，rsduck 会标记对象为 unavailable 并输出告警，不因为单个对象问题阻塞整个服务启动。全局 catalog 损坏、版本不支持、checksum 不一致等全局错误会阻止启动。

### 2.6 明确边界

rsduck 当前不提供：

- 完整 PostgreSQL transaction、planner、storage、replication 语义。
- 完整 PostgreSQL role/ACL、row-level security、column-level permission。
- MySQL wire protocol。
- MySQL catalog 兼容模型。
- DuckDB MVCC 时间旅行查询。
- 对 DuckDB 不支持能力的静默降级。

## 3. 给开发者看的设计

### 3.1 当前代码模块

当前代码按运行时职责划分：

```text
src/
  main.rs              进程启动、定时任务、服务生命周期
  config.rs            配置加载和默认值
  sql_route.rs         SQL 读写路由判断

  db/                  DuckDB engine、worker、SQL 执行、snapshot、restore
  catalog/             catalog 事实表、权限、mutation、分区、恢复校验
  pg_compat/           PG catalog / information_schema 兼容查询和函数改写
  server/              PG wire server 和 Web server
```

模块关系：

```text
server
  |
  v
db::execute_sql_as / db::authenticate_user / db::save_snapshot
  |
  +--> sql_route
  +--> pg_compat
  +--> catalog
  |
  v
DuckDB connections
```

`catalog`、`db`、`pg_compat` 当前通过 `include!` 做物理文件拆分，以保留原模块命名空间和私有可见性。后续可以在不改变行为的前提下逐步提升为真正的 Rust 子模块。

### 3.2 db 模块

`db` 是 DuckDB 执行层门面，主要职责：

- 初始化 in-memory DuckDB。
- 恢复 snapshot 或执行 `init_sql`。
- 创建 write/read/snapshot workers。
- 接收外部 SQL 请求并投递到正确 worker。
- 保存目录快照。
- 在启动和恢复后调用 catalog 校验。

内部文件：

| 文件 | 职责 |
|------|------|
| `db/model.rs` | `SqlResult`、worker command、全局 engine 状态 |
| `db/engine.rs` | `init_db`、engine 方法、对外 async API |
| `db/worker.rs` | read/write/snapshot worker 线程 |
| `db/execute.rs` | SQL 执行、catalog rewrite、授权和查询结果转换 |
| `db/snapshot.rs` | snapshot 导出、manifest、prefix 校验 |
| `db/restore.rs` | snapshot restore 和 init_sql 初始化 |

开发约束：

- 不要在 async runtime 里直接调用 DuckDB 阻塞 API。
- 不要绕过 write worker 执行写入、DDL 或 catalog mutation。
- snapshot worker 不处理用户 SQL。
- partition scheduler 不持有 DuckDB connection。

### 3.3 catalog 模块

`catalog` 是 rsduck 元数据事实来源和授权中心。所有受控 DDL、账号权限变更、分区维护都必须经过 catalog mutation contract。

内部结构：

| 目录/文件 | 职责 |
|-----------|------|
| `catalog/model.rs` | OID、内置 schema/role、内部结构体 |
| `catalog/storage.rs` | `rsduck_catalog.*` 物理表 DDL |
| `catalog/bootstrap.rs` | catalog bootstrap 和默认账号角色 |
| `catalog/journal.rs` | mutation journal、epoch、事务收尾 |
| `catalog/oid.rs` | 持久化 OID 分配 |
| `catalog/checksum.rs` | catalog checksum 计算和校验 |
| `catalog/recovery.rs` | 启动校验、unavailable 标记、分区入口恢复 |
| `catalog/guard.rs` | reserved schema guard、SQL 授权入口 |
| `catalog/lookup.rs` | catalog 查询 helper |
| `catalog/auth/` | 密码 hash、认证、principal、授权和权限函数 |
| `catalog/mutation/` | schema/table/view/index/alter/drop/comment/user/role/grant 等 mutation |
| `catalog/partition/` | 分区创建、路由、entrypoint、maintenance、repair、retention |

mutation 通用流程：

```text
1. normalize request
2. validate catalog contract
3. enter single write worker
4. BEGIN DuckDB transaction
5. insert rs_catalog_journal pending
6. mutate rsduck_catalog.*
7. execute DuckDB physical DDL / DML
8. run local consistency checks
9. mark journal applied
10. increment catalog_epoch and refresh checksum
11. COMMIT
```

开发约束：

- 任何 catalog 行为不得直接写 `rsduck_catalog.*` 而不写 journal。
- 不得直接创建或删除 `rsduck_internal.*`，必须通过 mutation planner。
- 不得把 DuckDB introspection 当作长期事实来源。
- 单对象损坏应隔离为 unavailable；全局 catalog 损坏才阻止启动。

### 3.4 pg_compat 模块

`pg_compat` 负责让 PG 客户端能读取元数据，但它不拥有真实元数据。

内部文件：

| 文件 | 职责 |
|------|------|
| `pg_compat/mod.rs` | `compat_result` 和 `rewrite_sql` 门面 |
| `pg_compat/show.rs` | `SHOW PARTITIONS` 解析和改写 |
| `pg_compat/rewrite.rs` | `pg_catalog.*` / `information_schema.*` relation 和函数改写 |
| `pg_compat/functions.rs` | 标量兼容函数 |
| `pg_compat/settings.rs` | PG session setting、SET/SHOW/current_setting |
| `pg_compat/projections.rs` | catalog projection SQL |

开发约束：

- projection 必须从 `rsduck_catalog.*` 派生。
- 不支持的 relation 要返回明确错误或定义好的空结果。
- 不允许退回到 DuckDB 内部 `duckdb_*` 表作为兼容 fallback。

### 3.5 server 和 sql_route 模块

`server` 负责网络入口：

- `server/pg.rs`：PostgreSQL wire protocol，认证后把 SQL 转给 `db`。
- `server/web.rs`：Web SQL Console、HTTP SQL API、登录、session、snapshot 触发。

`sql_route` 只做读写分类：

- 查询类 SQL 走 read worker。
- 写入、DDL、账号权限、分区维护走 write worker。
- managed partitioned table 的特殊 CREATE 语法在 route 前被识别为写请求。

开发约束：

- server 不直接访问 DuckDB connection。
- Web 分页只包装可分页查询。
- PG wire 和 Web 必须共用同一套 catalog 认证与授权。

## 4. 运行时流程

### 4.1 启动流程

```text
main
  -> load config
  -> find latest snapshot
  -> db::init_db
       -> open in-memory DuckDB
       -> restore snapshot or execute init_sql
       -> catalog::validate_after_start
       -> spawn write/read/snapshot workers
  -> spawn partition maintenance task
  -> spawn PG wire server
  -> start Web server
  -> on shutdown save snapshot
```

### 4.2 SQL 请求流程

```text
client
  -> server authenticates user
  -> db::execute_sql_as(username, sql)
  -> sql_route::route_sql
  -> read worker or write worker
  -> pg_compat rewrite if metadata query
  -> catalog guard and authorization
  -> DuckDB execute/query
  -> result returned to client
```

### 4.3 分区维护流程

```text
partition timer / write trigger / manual call
  -> compute maintenance job
  -> enqueue write task
  -> execute catalog mutation in write worker
  -> create / expire / repair physical partition
  -> refresh partition entrypoint
```

分区维护不直接持有 DuckDB connection，也不直接修改 catalog。

### 4.4 快照流程

```text
timer / manual / shutdown
  -> snapshot queue
  -> snapshot worker
  -> EXPORT DATABASE temporary dir
  -> write manifest
  -> rename to final snapshot dir
  -> cleanup expired snapshots
```

restore 只读取正式 snapshot 目录，不自动尝试更早 snapshot。

## 5. 配置视角

主要配置分组：

| 配置 | 作用 |
|------|------|
| `log_level` | 全局日志级别，允许 `trace`、`debug`、`info`、`warn`、`error`、`off` |
| `[db]` | init_sql、read workers、队列大小、最大返回行数 |
| `[snapshot]` | 是否启动恢复、目录、前缀、保存间隔、保留时间 |
| `[partition]` | 分区维护是否启用、维护间隔、校验间隔、每次任务上限 |
| `[pg]` | PG wire 监听地址 |
| `[web]` | Web 控制台是否启用和监听地址 |

开发者增加配置时应遵循当前产品形态：只暴露真实需要的参数，不保留未使用的兼容参数。

## 6. 测试和验收

现有测试覆盖以下关键路径：

- catalog bootstrap、默认 admin、认证授权。
- 普通表、视图、索引、约束、comment、drop、alter table。
- range partitioned table 创建、写入路由、null partition、retention、repair。
- `pg_catalog` / `information_schema` rewrite。
- `SHOW PARTITIONS`。
- reserved schema guard。
- snapshot restore、manifest 校验、startup consistency check。
- Web session cookie 和 PG-compatible 查询路径。

开发者修改核心模块后必须至少运行：

```powershell
cargo fmt --check
cargo test
```

文档或中文内容修改后，应额外做 UTF-8 解码和常见乱码扫描。

## 7. 文档关系

本文是项目设计入口：

- 使用者从第 2 章理解能力和边界。
- 开发者从第 3、4 章理解代码结构和运行链路。

深入文档：

- [duckdb-pool-design.md](duckdb-pool-design.md)：DuckDB 连接池、worker、队列、快照和分区调度细节。
- [rsduck_pg_catalog_design.md](rsduck_pg_catalog_design.md)：PG-compatible catalog、内部 catalog 表、mutation contract、权限、分区表和恢复校验细节。

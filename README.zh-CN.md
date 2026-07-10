# rsduck 开发者使用手册

语言：[English](README.md) | 中文

本文面向需要运行、接入、维护或继续开发 rsduck 的工程人员。内容以当前代码行为为准，重点说明可以做什么、不能做什么、失败时如何处理，以及新增能力时必须保持的约束。

## 1. 项目定位

rsduck 是一个基于 DuckDB 的内存数据库服务，对外提供：

- MySQL wire 协议，便于 Navicat 和 MySQL 客户端连接。
- Web SQL 控制台，支持查询、分页、快照和 Parquet 表导入。
- `rsduck_catalog.rs_*` 元数据与权限体系。
- 普通表、视图、索引、用户、角色和受管范围分区表。
- Snapshot v2 持久化与恢复。

rsduck 不是 MySQL，也不是将 DuckDB 的所有原生能力直接透传出去的代理。它采用以下原则：

1. DuckDB 保存物理对象和业务数据。
2. `rsduck_catalog.rs_*` 是受管对象、权限、依赖和快照元数据的唯一事实来源。
3. 所有对外 DDL 必须同时修改 DuckDB 物理对象和 rsduck catalog。
4. 不支持的能力直接返回错误，不回退到 DuckDB 内部 catalog 或旧实现路径。
5. 外部 SQL 一次只允许一条语句。

## 2. 核心架构

```text
MySQL client / Navicat          Web console
           |                         |
           +-----------+-------------+
                       |
                 SQL route + auth
                       |
          +------------+-------------+
          |                          |
    read worker pool             write worker
    N DuckDB connections         1 DuckDB connection
          |                          |
          +-------------+------------+
                        |
               in-memory DuckDB
          +-------------+-------------+
          |                           |
   business objects         rsduck_catalog.rs_*
                                      |
                              snapshot worker
```

运行时连接模型：

- 一个基础内存 DuckDB 实例。
- 一个串行写 worker。
- `db.read_workers` 个读 worker，查询按轮询方式分配。
- 一个独立快照 worker。
- 写入与快照通过同一个 gate 串行，避免导出过程中 catalog 或业务数据变化。

这意味着：

- 两次独立查询不保证落到同一个读连接。
- DuckDB 临时表是连接级对象，不能在两个外部请求之间可靠复用。
- 显式事务不能跨 Web/MySQL 请求使用。
- 程序内部可以设计“固定在同一 worker 的复合任务”，但不能将这种能力等同于外部多语句 SQL。

## 3. 快速启动

### 3.1 环境要求

- Rust stable 工具链。
- Windows PowerShell 或其他可以运行 Cargo 的终端。
- 不需要单独安装 DuckDB，项目使用 `duckdb` crate 的 `bundled` 特性。

### 3.2 开发模式

```powershell
cargo build
cargo run
```

服务从**当前工作目录**读取以下文件和目录：

- `rsduck.toml`
- `init.sql`
- `snapshot/`
- `logs/`
- `.rsduck.lock`

因此不要从不确定的工作目录启动可执行文件。Windows 服务也必须配置正确的 working directory。

仓库自带 `rsduck.toml` 使用：

```text
MySQL: 127.0.0.1:13306
Web:   http://127.0.0.1:13307
```

如果没有 `rsduck.toml`，代码默认 Web 端口同样是 `13307`。

初始管理员账号：

```text
username: admin
password: admin
```

首次启动后立即修改密码：

```sql
ALTER USER admin PASSWORD 'replace_with_a_strong_password';
```

### 3.3 停止服务

优先使用正常终止信号。正常关闭会先保存一次快照，再停止 worker。

强制结束进程可能跳过关闭快照。进程异常退出后如果残留 `.rsduck.lock`，先确认记录的 PID 已不存在，再处理锁文件；不要在仍有实例运行时删除锁文件并启动第二个实例。

## 4. 配置说明

完整配置示例：

```toml
[log]
level = "info"
dir = "logs"
file_prefix = "rsduck"
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

[mysql]
bind = "127.0.0.1:13306"

[web]
enabled = true
bind = "127.0.0.1:13307"
parquet_import_root = "."
```

### 4.1 配置规则

- 配置结构使用 `deny_unknown_fields`。拼错字段名会导致启动失败，不会静默忽略。
- 相对路径以进程工作目录为基准。
- `read_workers` 最少按 1 处理。
- 各队列达到上限时返回明确的 queue full 错误，不自动切换到其他执行路径。
- `max_result_rows` 是单次服务端结果上限，不等同于 Web 页大小。
- `snapshot.prefix` 只允许安全的快照目录前缀；非法前缀会阻止启动。
- `parquet_import_root` 是 Web Parquet 导入允许访问的根目录。导入请求只能使用该目录下的相对路径。

### 4.2 启动数据来源

启动顺序如下：

1. 获取 `.rsduck.lock`，防止同一工作目录启动多个实例。
2. 如果 `restore_on_startup = true`，查找最新的有效 Snapshot v2。
3. 找到快照时，从快照恢复 catalog 和业务对象。
4. 没有快照时，创建全新的 rsduck catalog。
5. 新库且 `db.init_sql` 非空时，执行初始化 SQL。
6. 启动读、写、快照 worker，以及 MySQL/Web 服务。

`init.sql` 是内部初始化入口，可以包含多条语句。它不受外部“一次一条 SQL”的限制，但每条 DDL 仍走 catalog-aware mutation。

## 5. Catalog 规则

### 5.1 唯一事实来源

受管元数据保存在 `rsduck_catalog`：

```text
rs_catalog_version   catalog 版本、epoch、checksum
rs_oid_alloc         OID 分配器
rs_catalog_journal   catalog 变更日志
rs_schema            schema
rs_type              类型
rs_relation          表、视图、索引等 relation
rs_column            列
rs_column_default    默认值
rs_constraint        主键、唯一、外键、检查约束
rs_index             索引
rs_dependency        对象依赖
rs_comment           注释
rs_relation_ext      rsduck 扩展属性
rs_partition         分区状态
rs_user              用户
rs_role              角色
rs_user_role         用户角色关系
rs_privilege         权限
```

禁止从外部直接写这些表。以下 schema 都是保留区域：

```text
rsduck_catalog
rsduck_internal
pg_catalog
information_schema
```

`information_schema`、`SHOW ...` 和 Navicat 使用的 MySQL 系统表是受控投影，不是可写 catalog。

### 5.2 DDL 必须保持的原子性

任何新增 DDL 或管理命令必须同时处理：

1. 权限校验。
2. catalog journal pending 记录。
3. DuckDB 物理对象变更。
4. `rs_relation`、`rs_column`、依赖等 catalog 记录。
5. journal 完成状态。
6. catalog epoch 和 checksum。
7. 失败时回滚物理对象和 catalog。

不要先执行原生 DuckDB DDL，再“尽量补 catalog”。这会产生 Navicat 不可见、权限失效或快照漏表的问题。

## 6. SQL 执行规则

### 6.1 外部一次只允许一条语句

允许：

```sql
SELECT * FROM main.kline_day;
```

拒绝：

```sql
SELECT 1;
SELECT 2;
```

拒绝多语句是 rsduck 路由层的产品约束，不是 DuckDB 原生限制。原因包括：

- 一次请求只能确定一个读写路由。
- Web API 当前只返回一个结果集。
- MySQL multi-results 没有作为公开协议能力实现。
- 每条语句都必须独立鉴权。
- 不允许事务或临时对象泄漏到后续用户请求。

### 6.2 不要跨请求使用显式事务

以下用法不属于受支持契约：

```sql
BEGIN;
```

然后在下一次请求中执行：

```sql
INSERT INTO ...;
COMMIT;
```

服务端 worker 连接由多个请求共享，外部请求没有事务连接绑定。需要原子操作时，应在程序内部新增明确的复合命令并在一个 worker 调用中完成。

### 6.3 临时表

DuckDB 原生支持 `CREATE TEMP TABLE`，但 rsduck 当前禁止从 Web/MySQL 创建临时表：

```sql
CREATE TEMP TABLE temp_t AS SELECT ...; -- 外部入口不支持
```

程序内部可以在同一个 worker、同一个 `Connection` 中顺序执行前置 SQL、主查询和清理 SQL。推荐使用类型明确的内部命令，而不是开放通用多语句字符串：

```rust
// 设计示例，不是当前公开 API
BEGIN;
CREATE TEMP TABLE temp_t AS ...;
SELECT ... FROM temp_t;
ROLLBACK;
```

适用于调度任务的前置 SQL、中间结果复用和复杂分析。要求：

- 整个任务固定在同一连接。
- 前置 SQL 只能修改临时对象。
- 无论成功失败都回滚或清理。
- 最终只返回主查询结果。
- 不把临时表登记到 rsduck catalog 或快照。

只使用一次的中间结果优先写成 CTE：

```sql
WITH prepared AS MATERIALIZED (
    SELECT code, avg(close) AS avg_close
    FROM main.kline_day
    GROUP BY code
)
SELECT * FROM prepared WHERE avg_close > 10;
```

## 7. 当前支持的对象与限制

### 7.1 普通查询和 DML

主要支持：

- `SELECT`、`WITH`、`EXPLAIN`
- `SHOW TABLES`、`SHOW COLUMNS`、`SHOW INDEX`
- `DESCRIBE`
- `INSERT`、`UPDATE`、`DELETE`
- `COPY table TO ...`
- `COPY table FROM ...`

查询会进入读 worker，写操作进入串行写 worker。

### 7.2 DDL 支持矩阵

| 能力 | 状态 | 约束 |
|---|---|---|
| `CREATE SCHEMA` | 支持 | 需要 system `manage_catalog` |
| `CREATE TABLE` | 支持 | 必须是受管普通表 |
| `CREATE TABLE AS SELECT` | 不支持外部 SQL | Parquet Web 导入使用专用 catalog-aware 实现 |
| `CREATE TEMP TABLE` | 不支持外部 SQL | 仅建议程序内部同连接任务使用 |
| `ALTER TABLE ADD COLUMN` | 支持 | 不支持指定列位置 |
| `ALTER TABLE DROP COLUMN` | 支持 | 受约束、索引、外键、分区键依赖保护 |
| `CREATE VIEW` | 支持 | 不支持 temporary 和 `OR REPLACE` |
| `CREATE INDEX` | 支持 | 必须显式命名；不支持 partial、INCLUDE、表达式索引 |
| `COMMENT ON` | 支持 | schema、table、view、index、column |
| `CREATE USER/ROLE` | 支持 | 用户必须设置密码 |
| `GRANT/REVOKE` | 支持 | 使用 rsduck 映射后的 read/write/ddl/system 权限 |
| 受管范围分区表 | 支持 | 使用 rsduck 扩展语法 |

### 7.3 受管列类型

创建 catalog 管理的表时，当前支持：

```text
BOOLEAN
SMALLINT
INTEGER
BIGINT
REAL
DOUBLE
DECIMAL / NUMERIC
VARCHAR / TEXT
DATE
TIME
TIMESTAMP
```

复杂列类型支持：

```text
<simple_type>[]
STRUCT(field_name <simple_type>, ...)
MAP(<simple_type>, <simple_type>)
```

rsduck 支持 DuckDB 原生复杂列类型，但不允许复杂类型嵌套复杂类型。复杂列内部只能使用简单标量类型，查询结果统一序列化为 JSON。

复杂列可以作为普通数据列使用，但暂不支持作为主键、唯一键、索引列、外键、分区键，也不支持非 `NULL` 默认值。DuckDB 还支持更多类型，但如果 rsduck catalog 没有类型映射，DDL 或 Parquet 导入会失败并回滚。不要依赖原生 DuckDB 能创建某类型，就假设 rsduck 受管表也支持。

## 8. 普通表开发案例

### 8.1 创建 schema 和表

```sql
CREATE SCHEMA market;
```

```sql
CREATE TABLE market.daily_quote (
    code       VARCHAR NOT NULL,
    trade_date DATE NOT NULL,
    open       DOUBLE,
    high       DOUBLE,
    low        DOUBLE,
    close      DOUBLE,
    volume     BIGINT,
    PRIMARY KEY (code, trade_date)
);
```

### 8.2 写入和查询

```sql
INSERT INTO market.daily_quote
    (code, trade_date, open, high, low, close, volume)
VALUES
    ('600000', DATE '2026-07-10', 10.1, 10.8, 9.9, 10.6, 1200000);
```

```sql
SELECT code, trade_date, close
FROM market.daily_quote
WHERE code = '600000'
ORDER BY trade_date DESC
LIMIT 100;
```

### 8.3 索引、视图和注释

```sql
CREATE INDEX idx_daily_quote_date
ON market.daily_quote(trade_date);
```

```sql
CREATE VIEW market.latest_quote AS
SELECT code, max(trade_date) AS latest_date
FROM market.daily_quote
GROUP BY code;
```

```sql
COMMENT ON TABLE market.daily_quote IS 'daily unadjusted quotes';
```

```sql
COMMENT ON COLUMN market.daily_quote.close IS 'closing price';
```

## 9. 用户、角色和权限案例

内置角色：

```text
admin     全部管理能力
operator  快照和 catalog 运维
ddl       预定义 DDL 角色名称；实际访问仍以 privilege 记录为准
writer    预定义写角色名称；实际访问仍以 privilege 记录为准
reader    预定义读角色名称；实际访问仍以 privilege 记录为准
```

创建用户和角色：

```sql
CREATE USER quant_reader PASSWORD='replace_me';
```

```sql
CREATE ROLE analyst;
```

授权关系读取并将角色授予用户：

```sql
GRANT SELECT ON TABLE market.daily_quote TO ROLE analyst;
```

```sql
GRANT ROLE analyst TO quant_reader;
```

直接向用户授权：

```sql
GRANT SELECT ON TABLE market.daily_quote TO quant_reader;
GRANT INSERT ON TABLE market.daily_quote TO quant_reader;
GRANT CREATE ON SCHEMA market TO quant_reader;
```

撤销：

```sql
REVOKE SELECT ON TABLE market.daily_quote FROM quant_reader;
REVOKE ROLE analyst FROM quant_reader;
```

权限映射：

- relation `SELECT/READ/USAGE` -> `read`
- relation `INSERT/UPDATE/DELETE` -> `write`
- relation `CREATE/DROP/OWNERSHIP` -> `ddl`
- schema `SELECT/READ/USAGE` -> `read`
- schema `CREATE/DROP/OWNERSHIP` -> `ddl`
- system 管理动作 -> `manage_snapshot`、`manage_catalog`、`manage_user`

不要直接修改 `rs_privilege`。必须通过 `GRANT/REVOKE`，保证 journal、checksum 和审计一致。

## 10. 受管范围分区表

创建按天分区并保留 30 个分区：

```sql
CREATE TABLE ods_access_log (
    id          BIGINT,
    access_time TIMESTAMP NOT NULL,
    content     TEXT
)
PARTITION BY RANGE (access_time)
WITH (
    partition_unit = 'day',
    retention = '30'
);
```

写入父表：

```sql
INSERT INTO ods_access_log(id, access_time, content)
VALUES (1, TIMESTAMP '2026-07-10 10:00:00', 'ok');
```

rsduck 会：

1. 根据分区键计算分区值。
2. 在 `rsduck_internal` 创建物理分区。
3. 写入 `rs_partition`。
4. 刷新父表查询入口。
5. 按 retention 规则清理过期分区。

禁止直接操作 `rsduck_internal` 中的物理分区。

维护命令：

```sql
CALL rsduck_run_partition_maintenance();
```

```sql
CALL rsduck_mark_partition_unavailable(
    'ods_access_log',
    '20260710',
    'manual reason'
);
```

```sql
CALL rsduck_repair_partition('ods_access_log', '20260710');
```

这些命令需要 `manage_catalog`。

## 11. Web Parquet 表导入

Web 左侧的 **Import Parquet** 用于将已有 Parquet 数据复制到 rsduck 内存数据库，并登记为 catalog 管理的普通表。

### 11.1 输入模型

- 单个 `.parquet` 文件表示一张逻辑表。
- 选择目录时，导入该目录顶层的所有 `.parquet` 文件。
- 目录模式按“一文件一表”处理。
- 默认使用文件名（不含扩展名）作为表名。
- 仅单文件模式允许指定自定义目标表名。
- 每批最多 256 个文件。

如果多个 Parquet 文件是同一逻辑表的分片，当前目录模式会将它们识别为多张表，不会自动 union。此类数据需要先合并，或后续实现显式的 Parquet Dataset 导入模式。

### 11.2 路径规则

配置：

```toml
[web]
parquet_import_root = "D:/data/rsduck-import"
```

Web 中只能填写相对于该根目录的路径：

```text
single/quotes.parquet
batch_20260710
```

不允许绝对路径，也不允许使用 `..` 或符号链接逃逸根目录。

### 11.3 导入语义

- 全部创建为普通表，`managed_kind = ordinary`。
- 数据复制进内存 DuckDB，成功后不依赖源文件继续存在。
- 不覆盖已有表。
- 批量导入是原子的；任意文件失败时整批回滚。
- Parquet 只提供数据和列类型，不恢复主键、索引、注释、owner 或原数据库权限。
- 不支持的列类型会导致整批失败。

导入完成后，根据业务需要单独创建索引、约束、注释和权限。

## 12. Snapshot v2

快照目录结构：

```text
snapshot/
  rsduck_20260710_120000/
    manifest.json
    catalog.duckdb
    data/
      10005.parquet
      10022.parquet
```

文件职责：

- `manifest.json`：格式版本、快照名称、catalog epoch/checksum、表文件、行数、视图和 macro DDL/checksum。
- `catalog.duckdb`：单个 DuckDB 文件，包含全部 `rs_*` catalog 表。
- `data/*.parquet`：每个普通物理 relation 一份业务数据。

### 12.1 保存时机

- 按 `snapshot.interval_secs` 周期保存。
- Web 右上角 **Save Snapshot** 手工保存。
- 正常关闭前保存。

保存期间与写 worker 串行。导出使用临时目录，成功后原子重命名；失败时清理临时目录。

### 12.2 恢复规则

恢复顺序：

1. catalog
2. schema
3. 普通表数据
4. 索引
5. 视图
6. macro/函数
7. checksum 和 catalog/物理一致性校验

关键失败行为：

- catalog 格式版本不匹配：启动失败。
- manifest 与 catalog epoch/checksum 不匹配：启动失败。
- 表数据行数不匹配：恢复失败。
- 视图或 macro DDL checksum 被篡改：恢复失败。
- 某个业务数据文件缺失：对应 relation 标记为 unavailable，其他对象继续恢复。

### 12.3 快照保留

周期任务会按 `retain_hours` 删除过期的最终快照目录。只识别符合 `{prefix}_YYYYMMDD_HHMMSS` 规则的目录，不把 `.tmp` 目录当作有效快照。

### 12.4 离线重置管理员密码

先停止服务，再执行：

```powershell
rsduck reset-admin-password --password <new_password>
```

不传 `--password` 时会重置为 `admin`，只适用于明确的本地恢复场景：

```powershell
rsduck reset-admin-password
```

命令会基于现有 Snapshot v2 生成一份新快照，不直接修改正在运行的内存实例。

## 13. MySQL/Navicat 接入

连接参数：

```text
host:     127.0.0.1
port:     13306
username: admin
password: configured password
database: main
```

支持的主要协议能力：

- 认证
- 普通查询
- prepared statement
- `SHOW TABLES`
- `SHOW COLUMNS`
- `SHOW INDEX`
- 常用 `information_schema` 探测
- Navicat 用户、角色和权限元数据探测
- DuckDB 视图和 macro/函数的 MySQL 展示投影

当前明确支持的 `information_schema` relation 包括：

```text
schemata
tables
views
routines
parameters
columns
statistics
table_constraints
key_column_usage
```

未支持的 `information_schema`、`pg_catalog` 或 MySQL 系统 relation 会返回明确错误，不会直接放行到 DuckDB 内部 catalog。

Navicat 展示的是兼容投影。不要根据界面中出现 MySQL 字段，就假设 rsduck 实现了 MySQL 存储引擎、事件、触发器或全部权限语义。

## 14. Web 控制台和 API

主要端点：

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

会话通过 HttpOnly、SameSite=Lax cookie 保存。Web API 不是无认证管理接口。

`POST /sql` 请求：

```json
{
  "sql": "SELECT * FROM main.kline_day",
  "page": 0,
  "page_size": 100
}
```

响应：

```json
{
  "columns": [
    {
      "name": "code",
      "sql_type": "text",
      "mysql_type": "varchar"
    }
  ],
  "rows": [["600000"]],
  "success": true,
  "msg": "ok"
}
```

Web 只对顶层没有 `LIMIT/OFFSET` 的 `SELECT/WITH` 自动增加分页包装。已经显式分页的 SQL 保持原样。

## 15. 常见错误与处理

### `relation does not exist in catalog`

含义：DuckDB 物理对象和 rsduck catalog 不一致，或者客户端访问了未登记对象。

处理：

1. 不要通过绕过 catalog 的原生连接创建业务表。
2. 检查 `rs_relation`、`rs_schema` 和 DuckDB `duckdb_tables()` 是否一致。
3. 如果来自旧数据，使用受管 Parquet 导入，不要手工补 catalog 行。

### `only one SQL statement is supported`

一次请求包含多条语句。拆成多个独立请求；需要原子或同连接语义时，新增程序内部复合命令。

### `unsupported DuckDB type for rsduck catalog`

物理类型没有 rsduck catalog 映射。不要绕过错误继续创建；应先增加完整类型映射、MySQL 展示类型、快照和测试。

### `reserved schema is managed by rsduck`

外部 SQL 尝试修改保留 schema。改用公开 DDL、管理命令或只读投影。

### `catalog checksum mismatch`

catalog 可能被绕过正常 mutation 直接修改，或者快照损坏。停止继续写入，保留日志和快照进行排查；不要直接覆盖 checksum。

### queue full

对应 worker 队列已满。先确认是否有慢查询、快照或大批写入阻塞，再评估队列大小。不要用自动切换读写 worker 的方式隐藏压力。

### `.rsduck.lock` 已存在

同一工作目录已有实例或上次异常退出。读取锁文件中的 PID；PID 仍存在时禁止启动第二个实例。

## 16. 开发工作流

修改前先确认工作树，避免覆盖用户已有修改：

```powershell
git status --short
```

格式化和静态检查：

```powershell
cargo fmt --all
cargo check
```

完整测试：

```powershell
cargo test
```

当前测试覆盖重点包括：

- catalog bootstrap、checksum 和恢复
- 用户、角色、权限
- 普通表、约束、视图、索引、注释
- 分区创建、写入、保留、修复
- Snapshot v2 保存和恢复
- MySQL 协议和 metadata 投影
- Web 分页和 Parquet 导入
- 批量 Parquet 导入失败时整批回滚

### 16.1 新增 DDL 的检查清单

新增或扩展 DDL 时至少检查：

```text
[ ] sqlparser 能解析
[ ] route_sql 读写路由正确
[ ] authorize_sql 权限正确
[ ] DuckDB 物理变更在 catalog transaction 内
[ ] rs_* 元数据完整
[ ] dependency 完整
[ ] journal、epoch、checksum 更新
[ ] 失败无物理/catalog 残留
[ ] information_schema/Navicat 投影正确
[ ] Snapshot v2 可保存和恢复
[ ] 普通、权限拒绝、回滚测试完整
```

### 16.2 新增程序内部复合任务

用于调度前置 SQL或临时表时，不要开放任意多语句接口。新增明确的 `SqlCommand` variant，并保证：

```text
[ ] 整个任务固定到一个 worker
[ ] 每段 SQL 有明确用途和权限边界
[ ] 读任务不能修改普通持久对象
[ ] 使用唯一临时表名
[ ] 成功、失败、取消都执行清理/回滚
[ ] 只返回一个定义明确的最终结果
[ ] 不污染后续复用该连接的用户请求
```

### 16.3 修改 MySQL 兼容层

- 优先基于 `rs_*` 和 DuckDB 官方 metadata table function 构建受控投影。
- 不创建一套 MySQL 影子 catalog 表作为新事实来源。
- 不为未支持 relation 添加静默空结果，除非客户端协议明确需要且产品已确认。
- 添加 Navicat 实际查询样本和协议测试。

### 16.4 修改快照格式

- 提升 `snapshot_format_version`。
- 旧格式快照不做自动兼容；数据导入走明确的 Parquet 导入入口。
- 更新 manifest 校验、恢复顺序和篡改测试。
- catalog 文件继续保持单文件，业务表数据继续按 relation 分离。

## 17. Windows 服务和发布

相关文件：

```text
packaging/windows-service/install-service.ps1
packaging/windows-service/uninstall-service.ps1
packaging/windows-service/rsduck-service.xml
packaging/windows-installer/rsduck.iss
.github/workflows/ci.yml
```

发布前至少执行：

```powershell
cargo fmt --all -- --check
cargo test
cargo build --release
```

服务部署必须同时确认：

- 可执行文件版本。
- working directory。
- `rsduck.toml`。
- `snapshot/` 和 `logs/` 的读写权限。
- MySQL/Web 监听地址是否只暴露到预期网络。
- 初始管理员密码已经修改。

## 18. 当前边界总结

以下行为是当前明确的产品边界：

- 对外客户端连接使用 MySQL 兼容协议。
- catalog 只使用 `rsduck_catalog.rs_*`。
- 外部一次只执行一条 SQL。
- 外部不支持临时表和跨请求事务。
- 外部 `CREATE TABLE AS SELECT` 不支持。
- Parquet 导入必须走 Web 受管导入入口。
- 单个 Parquet 文件对应一张逻辑表。
- 不支持的 catalog relation、类型或 DDL 直接报错。
- 缺失依赖不自动回退到旧路径。
- 所有可恢复状态必须进入 Snapshot v2。

继续开发时，如果一个方案会绕过这些边界，应先修改产品设计和测试契约，而不是在局部代码中增加兼容分支。

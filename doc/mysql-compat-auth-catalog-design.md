# rsduck MySQL 兼容、鉴权与 Catalog 管理设计

语言：中文 | [English](mysql-compat-auth-catalog-design.en.md)

本文说明 rsduck 如何在 DuckDB 之上提供 MySQL 兼容体验，以及鉴权、权限和自身 catalog 的管理原则。重点不是逐项复刻 MySQL，而是解释哪些能力被投影、哪些能力被拒绝，以及为什么 `rsduck_catalog.rs_*` 必须是唯一事实来源。

## 1. 设计定位

rsduck 对外提供 MySQL wire 协议，主要目标是：

- 让 Navicat 和常见 MySQL 客户端可以连接。
- 支持常见查询、prepared statement、`SHOW ...` 和 metadata 探测。
- 支持用户、角色、权限的基本管理。
- 让客户端看到的表、视图、函数、权限来自 rsduck 当前状态。

它不追求完整 MySQL 语义：

- 不实现 MySQL 存储引擎。
- 不实现 MySQL 事件、触发器、完整系统库。
- 不把 `mysql.*` 表作为真实 catalog。
- 不把 `information_schema` 写成第二套事实来源。

核心原则是：

```text
MySQL protocol compatibility is an adapter.
rsduck_catalog.rs_* is the source of truth.
DuckDB is the physical execution engine.
```

## 2. MySQL 协议层

MySQL 服务由 `src/server/mysql` 实现，监听配置中的 `[mysql].bind`。

连接建立后流程如下：

```text
TCP accept
  -> send handshake
  -> receive auth response
  -> authenticate against rsduck catalog
  -> create MySqlSession
  -> command loop
```

command loop 主要处理：

```text
COM_QUERY
COM_STMT_PREPARE
COM_STMT_EXECUTE
COM_STMT_CLOSE
COM_STMT_RESET
COM_PING
COM_INIT_DB
COM_QUIT
```

不支持的命令返回 MySQL error packet，而不是吞掉或伪装成功。

## 3. Session 模型

MySQL session 保存：

- username
- current database
- prepared statement map
- next statement id

`COM_INIT_DB` 只改变 session database。rsduck 没有 MySQL 多 database 存储模型，实际 schema 解释规则是：

- 空 database 或 `memory` 映射为 `main`。
- 其他 database 名按 schema 名处理。

这让 Navicat 可以按 MySQL database 习惯操作，但内部仍是 DuckDB schema 和 rsduck catalog。

## 4. 认证设计

Web 和 MySQL 都通过 `DbHandle::authenticate` 进入同一套 catalog 认证逻辑。认证请求包含：

- 协议类型。
- 用户名。
- 明文密码或 MySQL challenge response。

用户保存在：

```text
rsduck_catalog.rs_user
```

关键字段包括：

- `username`
- `password_hash`
- `password_algo`
- `mysql_auth_plugin`
- `mysql_auth_string`
- `status`
- `last_login_at`

rsduck 同时保存服务端自身使用的密码 hash，以及 MySQL 协议需要的 verifier。创建用户或修改密码时，两类凭据必须一起更新。

```sql
CREATE USER quant_reader PASSWORD='replace_me';
ALTER USER quant_reader PASSWORD 'new_password';
```

默认管理员 `admin/admin` 仅用于初始化。生产或长期开发环境启动后应立即修改。

## 5. 鉴权模型

鉴权以 `SessionPrincipal` 为核心，包含：

- user id
- username
- roles

管理员判断很直接：拥有 `admin` 角色即视为管理员。

普通权限检查分为三类：

```text
system   -> system action
schema   -> schema action
relation -> relation action
```

权限记录保存在：

```text
rsduck_catalog.rs_privilege
```

授权对象通过以下字段表达：

```text
principal_type  user / role
principal_id
object_type     system / schema / relation
object_id
action
```

检查时同时考虑：

- 用户直接权限。
- 用户拥有角色带来的权限。
- admin 角色短路通过。
- relation read 可以通过 schema read 继承。

权限拒绝会写审计日志：

```text
target: rsduck_audit
event: permission_denied
```

## 6. 权限动作映射

rsduck 不照搬 MySQL 的全部权限位，而是映射到内部动作：

```text
relation SELECT/READ/USAGE -> read
relation INSERT/UPDATE/DELETE -> write
relation CREATE/DROP/OWNERSHIP -> ddl

schema SELECT/READ/USAGE -> read
schema CREATE/DROP/OWNERSHIP -> ddl

system -> manage_snapshot / manage_catalog / manage_user
```

示例：

```sql
CREATE ROLE analyst;
GRANT SELECT ON TABLE market.daily_quote TO ROLE analyst;
GRANT ROLE analyst TO quant_reader;
```

或者直接授权给用户：

```sql
GRANT SELECT ON TABLE market.daily_quote TO quant_reader;
GRANT INSERT ON TABLE market.daily_quote TO quant_reader;
GRANT CREATE ON SCHEMA market TO quant_reader;
```

不要直接写 `rs_privilege`。必须通过 `GRANT/REVOKE`，让 journal、checksum、审计和权限投影保持一致。

## 7. SQL 鉴权入口

外部 SQL 进入执行层后，会经过：

```text
route_sql
  -> worker
  -> execute_typed_sql_blocking / describe_sql_blocking
  -> authorize_sql
  -> execute or reject
```

鉴权根据 statement 类型提取对象和动作：

- 查询表需要 relation `read`。
- 写表需要 relation `write`。
- 创建 schema/table/view/index 需要对应 schema 或 system 管理权限。
- 用户、角色、权限管理需要 system `manage_user` 或 admin。
- snapshot 需要 system `manage_snapshot` 或 admin。

DDL 的实际 catalog mutation 还会再次进入明确的执行函数，例如 create table、grant、drop 等。鉴权不是靠字符串黑名单完成的，而是围绕对象、动作和 catalog 记录。

## 8. Catalog 的事实来源设计

rsduck 的受管 metadata 保存在 `rsduck_catalog`：

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

保留 schema：

```text
rsduck_catalog
rsduck_internal
pg_catalog
information_schema
```

外部 SQL 不允许修改这些保留区域。`information_schema` 和 `mysql.*` 只是兼容投影，不是事实来源。

## 9. Catalog-Aware Mutation

DDL 不能直接透传给 DuckDB。`execute_catalog_aware_write_as` 会把受支持的 DDL 分派到明确 mutation：

```text
CREATE SCHEMA       -> create_schema
CREATE USER/ROLE    -> create_user_account / create_role_account
ALTER USER          -> alter_user_account
CREATE TABLE        -> create_table_relation
CREATE VIEW         -> create_view_relation
CREATE INDEX        -> create_index_relation
ALTER TABLE         -> alter_table_relation
DROP                -> drop_objects
COMMENT ON          -> comment_object
GRANT/REVOKE        -> grant_privileges / revoke_privileges
managed partition   -> create_range_partitioned_table
```

每个 catalog-aware mutation 必须维护：

1. 权限校验。
2. pending journal。
3. DuckDB 物理对象变更。
4. `rs_*` catalog 记录。
5. dependency。
6. journal completed。
7. epoch 和 checksum。
8. 失败回滚。

这个过程的目标是保证客户端看到的对象、权限判断、快照恢复和 DuckDB 物理状态一致。

## 10. 为什么不创建 MySQL 系统表

Navicat 会查询大量 MySQL 系统表，例如：

```text
mysql.user
mysql.db
mysql.role_edges
mysql.default_roles
mysql.tables_priv
mysql.columns_priv
mysql.procs_priv
```

rsduck 不在 DuckDB 中创建这些真实表，原因是：

- 它们会成为第二套 metadata，和 `rsduck_catalog` 竞争事实来源。
- MySQL 字段语义和 rsduck 权限模型不完全一致。
- 写入这些表无法自然触发 journal、checksum、snapshot 和权限校验。
- 未来维护成本会高于受控投影。

正确方式是把 Navicat 查询改写为子查询投影：

```text
mysql.user       -> projection over rs_user and rs_user_role
mysql.db         -> projection over rs_privilege and rs_schema
mysql.role_edges -> projection over rs_user_role
```

这些投影是只读的，字段值按 MySQL 客户端期望填充，但真实状态仍由 rsduck catalog 决定。

## 11. information_schema 投影

当前支持的 `information_schema` relation 包括：

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

投影来源不是单一表，而是组合：

- `rsduck_catalog.rs_*`
- DuckDB metadata table function
- 当前登录用户权限
- rsduck 扩展字段，例如 managed kind、availability status

例如表列表需要同时考虑：

- DuckDB 是否存在物理表。
- `rs_relation` 是否存在受管记录。
- relation 是否 available。
- 当前用户是否有 read 权限。
- schema 是否为保留 schema。

这种设计避免了早期问题：Navicat 展示了一个对象，但双击时 DuckDB 或 catalog 找不到对应 relation。

## 12. SHOW 语句兼容

MySQL 客户端常用：

```sql
SHOW TABLES;
SHOW COLUMNS FROM t;
SHOW INDEX FROM t;
SHOW ENGINES;
SHOW VARIABLES;
SELECT DATABASE();
SELECT VERSION();
```

rsduck 对这些请求分两类处理：

- 纯兼容常量结果，例如 `SHOW ENGINES`、`SHOW VARIABLES`、`SELECT VERSION()`。
- 改写为受控 catalog/information_schema 查询，例如 `SHOW TABLES`、`SHOW COLUMNS`、`SHOW INDEX`。

这样 Navicat 能正常加载对象树，但不会让客户端绕过 catalog 直接读取 DuckDB 内部系统表。

## 13. SQL 方言兼容

MySQL protocol 层在送入 DuckDB 前会做有限改写：

```text
`identifier`       -> "identifier"
LIMIT offset,count -> LIMIT count OFFSET offset
? placeholders     -> $1, $2, ...
```

这些改写只处理明确需要的兼容点。rsduck 不维护完整 MySQL parser，也不做大范围 SQL 语义转换。无法明确支持的 SQL 应返回错误。

## 14. Prepared Statement

prepared statement 流程：

```text
COM_STMT_PREPARE
  -> parse original SQL
  -> rewrite ? to $n
  -> apply MySQL compatibility rewrite
  -> describe_sql_with_params_as
  -> allocate statement id
  -> return parameter and column metadata

COM_STMT_EXECUTE
  -> parse binary parameters
  -> execute_typed_sql_with_params_as
  -> return binary result rows or OK packet
```

参数绑定在进入 DuckDB 前转换为 rsduck 内部 `SqlParam`，再由执行层完成绑定。返回列类型会映射为 MySQL column definition。

## 15. 类型展示

rsduck 内部结果使用中性类型：

```text
SqlType
SqlValue
SqlTypedResult
```

Web API 返回 `sql_type` 和 `mysql_type`。MySQL protocol 层把 `SqlType` 映射为 MySQL column type、charset、flags 和 decimals。

这意味着：

- 物理执行仍是 DuckDB。
- Web 展示不绑定 MySQL packet 细节。
- MySQL 客户端看到的是兼容类型。

新增类型时必须同时补齐：

- DuckDB -> rsduck catalog 类型映射。
- Web `sql_type/mysql_type` 展示。
- MySQL column type 映射。
- Snapshot 保存恢复。
- Parquet 导入。
- 测试。

## 16. Snapshot 与 Catalog 的关系

Snapshot v2 把 catalog 单独导出为：

```text
catalog.duckdb
```

业务数据按 relation 导出为：

```text
data/<rel_oid>.parquet
```

这样做的原因是：

- catalog 是元数据事实来源，需要整体保存。
- 业务表数据可以按 relation 拆分，便于行数校验和局部 unavailable 标记。
- MySQL 兼容投影不需要持久化，因为它可以从 catalog 和 DuckDB metadata 重新生成。

因此 `mysql.*`、`information_schema.*` 不应进入快照实体表。进入 snapshot 的是 rsduck catalog 和受管业务对象。

## 17. Navicat 兼容策略

Navicat 的对象树会触发很多 metadata 查询。rsduck 的策略是：

1. 只支持已经观测并确认必要的查询。
2. 用受控投影回答对象树、表、列、索引、视图、函数、权限相关查询。
3. 对不支持的 relation 返回明确错误，而不是创建空壳系统表。
4. 每补一个 Navicat 兼容点，都应加协议测试或至少保留查询样本。

这比“把 MySQL 系统表都建出来”更克制，也更符合 rsduck 的事实来源原则。

## 18. 安全边界

必须保持以下边界：

- Web API 必须先登录，不能变成无认证管理接口。
- MySQL 认证必须走 `rs_user`，不能接受匿名连接。
- 外部 SQL 不能直接写 `rsduck_catalog`、`rsduck_internal`、`information_schema`、`pg_catalog`。
- 用户、角色和权限只能通过 DDL 或管理命令修改。
- Snapshot 手工保存需要 `manage_snapshot`。
- Parquet 导入路径必须限制在 `web.parquet_import_root` 下。
- 未支持的 MySQL metadata relation 不应自动放行到 DuckDB 内部 catalog。

## 19. 新增兼容能力的开发规则

新增 MySQL/Navicat 兼容点时，按以下顺序判断：

1. 这个查询是客户端启动、对象树、编辑器、权限管理必须的吗？
2. 是否可以从 `rsduck_catalog.rs_*` 和 DuckDB 官方 metadata function 构造只读投影？
3. 是否会引入第二套事实来源？
4. 是否需要权限过滤？
5. 是否需要影响 Snapshot v2？
6. 是否有协议测试覆盖？

推荐实现方式：

```text
mysql_compat::rewrite_sql
  -> relation-specific projection SQL
  -> existing DbHandle execution path
```

不推荐实现方式：

```text
CREATE TABLE mysql.user (...)
INSERT fake rows
let client query it directly
```

## 20. 总结

rsduck 的 MySQL 兼容层本质是一个适配器：

- 协议上尽量满足 MySQL 客户端。
- 元数据上始终回到 `rsduck_catalog.rs_*`。
- 执行上始终回到 DuckDB。
- 权限上始终通过 rsduck principal/privilege 判断。
- 持久化上始终进入 Snapshot v2。

这个边界清晰后，Navicat 兼容、Web 管理、Snapshot 恢复和内部 catalog 才能保持同一套语义。

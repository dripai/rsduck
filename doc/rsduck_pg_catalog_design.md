# rsduck PG-compatible Catalog 落地设计

项目设计总览已合并到 [README.zh-CN.md](../README.zh-CN.md#架构设计与项目设计)。本文是 catalog 深入设计，重点描述 PG-compatible 元数据模型、账号权限、metadata mutation、兼容查询和恢复校验。

本文定义 rsduck 的元数据 catalog 方案。它不是 PostgreSQL 内核复刻方案，也不是 MySQL catalog 兼容方案；它是 rsduck 当前目标版本必须稳定实现的产品契约。

DuckDB 仍然是唯一 SQL 执行引擎。rsduck catalog 的职责是维护对象元数据、对外提供 PG-compatible metadata 查询、驱动 managed relation 的生命周期，并在启动恢复时校验 DuckDB 物理对象和元数据一致。

DuckDB 连接池、单写多读、目录快照等执行层设计见 [duckdb-pool-design.md](duckdb-pool-design.md)。本文只讨论 catalog、账号权限、metadata mutation、兼容查询和恢复校验。

## 1. 设计结论

- rsduck 采用 PostgreSQL catalog object model 作为唯一元数据兼容模型。
- MySQL catalog 不作为内部事实来源，也不作为当前产品契约的一部分。
- `rsduck_catalog.*` 是内部事实来源，只允许 rsduck catalog planner 写入。
- `pg_catalog.*` 和 `information_schema.*` 是只读兼容投影，用于 PG 客户端、ORM、DBeaver、Navicat 等工具查询元数据。
- DuckDB 物理表、视图、索引必须和 `rsduck_catalog.*` 中的 active 元数据一致。
- catalog contract 内的行为必须真实、可恢复、可验证。
- catalog contract 外的行为必须返回明确错误或明确空结果，不做隐式 fallback。

## 2. 支持边界

### 2.1 支持内容

rsduck catalog 必须支持以下对象和能力：

- schema / namespace 元数据。
- table / view / index relation 元数据。
- column 元数据，包括顺序、类型、nullable、default、generated 标记。
- built-in type 和 relation row type 元数据。
- 最小真实账号与权限模型，包括连接认证、角色、schema/relation 级授权和管理操作授权。
- primary key、unique、check、foreign key 的约束元数据。
- index 元数据。
- default expression 元数据。
- object comment 元数据。
- relation dependency 元数据，包括 view 依赖 table、index 依赖 table、constraint 依赖 table/column。
- managed range partitioned table：用户查询稳定分区表，rsduck 按 `hour` / `day` / `month` / `year` 管理内部物理分区表。
- `pg_catalog.*` 和 `information_schema.*` 的只读查询。
- 启动恢复时校验 catalog 和 DuckDB 物理对象一致；table、view、index 等单对象级不一致必须输出明确告警并隔离该对象，但不阻塞服务启动。

### 2.2 不支持内容

rsduck 不实现以下能力：

- 完整 PostgreSQL kernel。
- 完整 PostgreSQL transaction、MVCC、planner、storage、replication、permission 语义。
- 完整 PostgreSQL role/ACL 语义、role inheritance、row-level security、column-level permission。
- 外部客户端直接写 `pg_catalog.*`、`information_schema.*`、`rsduck_catalog.*`。
- MySQL wire protocol。
- MySQL catalog 作为内部元数据模型。
- 未列入兼容矩阵的 catalog 查询自动改查 DuckDB 内部表。
- 对 DuckDB 不支持的类型、约束、DDL 做静默降级。

## 3. 架构分层

```text
PG client / Web SQL / management API
        |
        v
SQL classifier / catalog query rewriter / DDL planner
        |
        +-- readonly metadata projection
        |       |
        |       v
        |  pg_catalog.* / information_schema.*
        |
        +-- catalog mutation
                |
                v
        rsduck_catalog.* internal source of truth
                |
                v
        DuckDB physical schemas, tables, views, indexes
```

各层职责：

| 层 | 职责 |
|----|------|
| SQL classifier | 识别普通 SQL、catalog 查询、reserved schema 访问和受控 DDL。 |
| catalog query rewriter | 将支持范围内的 `pg_catalog.*` / `information_schema.*` 查询改写到只读投影。 |
| DDL planner | 将受控 DDL 或 management API 请求转换成 catalog mutation。 |
| `rsduck_catalog.*` | 保存内部事实来源。所有 active 对象必须能从这里还原。 |
| DuckDB physical layer | 执行真实 table/view/index/constraint DDL 和用户查询。 |

`duckdb_tables()`、`duckdb_columns()` 等 DuckDB introspection function 只能用于启动校验和诊断，不作为长期事实来源。正常运行时，catalog 查询必须从 `rsduck_catalog.*` 派生。

### 3.1 代码模块拆分目标

当前实现必须按功能边界拆分，避免把 catalog、PG 兼容、连接池和 Web UI 继续堆在少数大文件中。拆分目标不是改变产品能力，而是让本文定义的 catalog contract 可以被稳定实现、测试和维护。

拆分原则：

- 保留稳定门面 API。外部模块仍通过 `catalog::bootstrap_fresh`、`catalog::validate_after_start`、`catalog::execute_catalog_aware_write_as`、`catalog::authorize_sql`、`catalog::authenticate_user` 等入口访问 catalog。
- 子模块按运行时职责拆分，不按文档章节机械拆分。
- catalog mutation 的事务入口必须保持集中，不能让各功能模块绕过 `run_catalog_tx`、journal、epoch 和 checksum 约束。
- 分区维护、用户管理、权限管理、DDL mutation 都必须复用同一套 catalog mutation contract。
- `pg_compat` 只负责兼容结果、SQL rewrite 和只读 projection SQL，不直接修改 catalog。
- `db` 只负责 DuckDB 连接池、worker、SQL 调度、snapshot 和 restore，不承载 catalog 业务规则。

目标目录结构：

```text
src/
  catalog/
    mod.rs
    model.rs
    bootstrap.rs
    storage.rs
    journal.rs
    oid.rs
    checksum.rs
    recovery.rs
    guard.rs
    lookup.rs
    sql_util.rs

    auth/
      mod.rs
      password.rs
      principal.rs
      authorize.rs
      privilege.rs

    mutation/
      mod.rs
      executor.rs
      parser.rs
      schema.rs
      relation.rs
      table.rs
      view.rs
      index.rs
      alter_table.rs
      drop.rs
      comment.rs
      user_role.rs
      grant.rs

    partition/
      mod.rs
      create.rs
      routing.rs
      entrypoint.rs
      maintenance.rs
      repair.rs
      retention.rs
      validation.rs

  pg_compat/
    mod.rs
    rewrite.rs
    show.rs
    functions.rs
    settings.rs
    projections.rs
    parser.rs

  db/
    mod.rs
    engine.rs
    worker.rs
    execute.rs
    snapshot.rs
    restore.rs

  server/
    pg.rs
    web.rs
```

模块职责：

| 模块 | 职责 |
|------|------|
| `catalog::model` | catalog 内部结构体、常量、对象类型、状态枚举。 |
| `catalog::bootstrap` | 创建 catalog storage、默认 schema、默认 `admin/admin`、内置 role 和 bootstrap rows。 |
| `catalog::storage` | `rsduck_catalog.*` 物理表 DDL。 |
| `catalog::journal` | journal 写入、完成、失败标记和 mutation 审计入口。 |
| `catalog::oid` | 持久化 OID 分配。 |
| `catalog::checksum` | catalog checksum 计算、刷新和校验。 |
| `catalog::recovery` | 启动恢复、一致性校验、unavailable 标记和分区 entrypoint 重建。 |
| `catalog::guard` | reserved schema guard、外部 SQL 防护、catalog projection 拦截。 |
| `catalog::lookup` | namespace、relation、column、user、role、privilege 等 catalog 查询 helper。 |
| `catalog::sql_util` | SQL 字符串转义、标识符引用、简单 SQL token 解析、通用 parser helper。 |
| `catalog::auth` | 密码 hash、认证、session principal、授权动作、权限函数计算。 |
| `catalog::mutation` | catalog mutation 统一执行入口，以及 schema/table/view/index/alter/drop/comment/user/role/grant/revoke 等受控变更。 |
| `catalog::partition` | range partitioned table 创建、写入路由、entrypoint、retention、repair 和维护任务。 |
| `pg_compat` | `pg_catalog` / `information_schema` projection、兼容函数、`SHOW PARTITIONS`、settings、SQL rewrite。 |
| `db` | DuckDB in-memory engine、read/write worker、SQL 执行、snapshot、restore。 |
| `server` | PG wire 和 Web HTTP 入口。 |

拆分顺序约束：

```text
1. 拆 pg_compat：纯 rewrite / projection / compat result，风险最低。
2. 拆 catalog 公共底座：model、sql_util、lookup、storage、journal、oid、checksum。
3. 拆 catalog auth：认证、principal、授权、权限函数。
4. 拆 catalog mutation：保持 executor 统一事务入口，再拆各 mutation handler。
5. 拆 catalog partition：保持 mutation contract 不变，再拆 create/routing/entrypoint/maintenance/repair/retention。
6. 拆 db：engine、worker、execute、snapshot、restore。
7. 拆 server：pg 和 web；Web 静态资源是否外置可单独处理。
```

每一步拆分都必须满足：

- 对外 public API 尽量不变。
- `cargo fmt` 必须通过。
- `cargo test` 必须通过。
- 不得在拆分过程中改变 catalog 行为语义。
- 不得因为移动模块引入 fallback、兼容分支或重复实现。

## 4. Schema 规则

保留 schema：

| schema | 用途 | 外部访问 |
|--------|------|----------|
| `pg_catalog` | PG-compatible 只读投影和函数。 | 只读 |
| `information_schema` | SQL 标准只读投影。 | 只读 |
| `rsduck_catalog` | 内部事实表。 | 禁止外部写入，默认禁止普通查询 |
| `rsduck_internal` | managed physical table、内部生成对象。 | 默认禁止普通查询 |

默认用户 schema 使用 DuckDB 默认 schema `main`。rsduck 不把 `main` 自动伪装成 `public`。如果业务需要 `public`，必须显式创建 `public` schema，并将对象建入 `public`。

命名规则：

- 用户对象不得创建在保留 schema。
- managed physical table 必须创建在 `rsduck_internal`。
- 分区表查询入口必须创建在用户 schema。
- 同一个 namespace 内 `relname` 必须唯一。
- 所有内部生成对象名必须可重复计算，不能依赖随机后缀。

managed range partitioned table 的物理表命名：

```text
rsduck_internal.{partitioned_relname}_{partition_value}
```

示例：

```text
main.ods_access_log
rsduck_internal.ods_access_log_20260701
rsduck_internal.ods_access_log_20260702
rsduck_internal.ods_access_log_null
```

## 5. 内部 Catalog 表

`pg_*` 表承载 PG object model。`rs_*` 表承载 rsduck 私有生命周期、版本、分区和 mutation 状态。`pg_*` 表字段优先沿用 PostgreSQL 命名，但只承诺本文定义的语义。

### 5.1 `rsduck_catalog.rs_catalog_version`

用途：记录 catalog schema 版本和当前一致性状态。

主键：

- `id`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `id` | BIGINT | 固定为 1。 |
| `schema_version` | BIGINT | catalog schema 版本。 |
| `catalog_epoch` | BIGINT | 每次完成 catalog mutation 后递增。 |
| `catalog_checksum` | VARCHAR | active catalog 状态校验值。 |
| `status` | VARCHAR | `initializing` / `ready` / `recovering` / `failed`。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `updated_at` | TIMESTAMP | 更新时间。 |

规则：

- 启动成功后 `status` 必须为 `ready`。
- `catalog_epoch` 只在 mutation commit 后递增。
- 校验失败时不得继续对外提供服务。

### 5.2 `rsduck_catalog.rs_oid_alloc`

用途：分配稳定 OID。

主键：

- `id`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `id` | BIGINT | 固定为 1。 |
| `next_oid` | BIGINT | 下一个可分配 OID。 |
| `updated_at` | TIMESTAMP | 更新时间。 |

规则：

- namespace、relation、type、constraint、index、default object 共用同一个 OID 空间。
- OID 持久化，重启后不得重算。
- DROP 后 OID 不复用。
- 禁止使用对象名 hash 生成 OID。

### 5.3 `rsduck_catalog.rs_catalog_journal`

用途：记录 catalog mutation，支持故障诊断和恢复校验。

主键：

- `journal_id`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `journal_id` | BIGINT | journal id，单调递增。 |
| `catalog_epoch` | BIGINT | mutation 完成后的 catalog epoch。 |
| `mutation_type` | VARCHAR | mutation 类型。 |
| `target_oid` | BIGINT | 主要对象 OID。 |
| `request_json` | VARCHAR | 标准化请求参数。 |
| `status` | VARCHAR | `pending` / `applied` / `failed`。 |
| `error_message` | VARCHAR | 失败原因。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `applied_at` | TIMESTAMP | 成功应用时间。 |

规则：

- 每个 catalog mutation 必须写 journal。
- mutation 成功 commit 后 journal status 必须为 `applied`。
- 启动时发现 `pending` journal 必须执行恢复校验；不能直接忽略。

### 5.4 `rsduck_catalog.pg_namespace`

用途：记录 schema / namespace。

主键：

- `oid`

唯一约束：

- `nspname`

真实字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `oid` | BIGINT | namespace OID。 |
| `nspname` | VARCHAR | schema 名称。 |
| `nspowner` | BIGINT | owner OID，默认是创建者用户 OID。 |
| `nspacl` | VARCHAR | ACL 展示字段，当前为空字符串。 |

规则：

- 必须内置 `pg_catalog`、`information_schema`、`rsduck_catalog`、`rsduck_internal`、`main`。
- `nspowner` 只用于兼容展示，不作为授权判断来源。

### 5.5 `rsduck_catalog.pg_type`

用途：记录 built-in type 和 relation composite row type。

主键：

- `oid`

唯一约束：

- `(typnamespace, typname)`

真实字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `oid` | BIGINT | type OID。 |
| `typname` | VARCHAR | PG-compatible 类型名。 |
| `typnamespace` | BIGINT | namespace OID。 |
| `typowner` | BIGINT | owner OID，默认是创建者用户 OID。 |
| `typlen` | INT | PG-compatible 长度。 |
| `typbyval` | BOOLEAN | PG-compatible by-value 标记。 |
| `typtype` | VARCHAR | `b` built-in，`c` composite。 |
| `typcategory` | VARCHAR | PG-compatible 类型分类。 |
| `typisdefined` | BOOLEAN | 是否已定义。 |
| `typrelid` | BIGINT | composite type 对应 relation OID，非 composite 为 0。 |
| `typelem` | BIGINT | array element type，当前未使用为 0。 |
| `typarray` | BIGINT | array type OID，当前未使用为 0。 |
| `rsduck_physical_type` | VARCHAR | DuckDB 原始类型名。 |

规则：

- managed DDL 只能使用类型映射表中列出的类型。
- 未知 DuckDB 类型不得静默映射成 `text`。
- relation 创建时必须创建对应 composite row type，并写入 `pg_class.reltype`。

### 5.6 `rsduck_catalog.pg_class`

用途：记录所有 relation，包括 table、view、index 和 composite relation。

主键：

- `oid`

唯一约束：

- `(relnamespace, relname)`

真实字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `oid` | BIGINT | relation OID。 |
| `relname` | VARCHAR | relation 名称。 |
| `relnamespace` | BIGINT | namespace OID。 |
| `reltype` | BIGINT | relation composite row type OID，没有则为 0。 |
| `relowner` | BIGINT | owner OID，默认是创建者用户 OID。 |
| `relkind` | VARCHAR | relation kind。 |
| `relpersistence` | VARCHAR | 当前固定为 `p`。 |
| `relnatts` | INT | 用户字段数量。 |
| `reltuples` | DOUBLE | 估算行数。 |
| `relhasindex` | BOOLEAN | 是否存在 active index metadata。 |
| `relispartition` | BOOLEAN | 是否为 managed physical partition table。 |
| `relpartbound` | VARCHAR | 分区边界展示字段，managed range partition 写标准化边界表达式。 |
| `reloptions` | VARCHAR | key-value options，使用 `key=value` 列表。 |

`relkind` 支持值：

| 值 | 语义 |
|----|------|
| `r` | ordinary table |
| `i` | index |
| `v` | view |
| `m` | materialized view metadata，仅当 DuckDB 物理对象存在时可用 |
| `c` | composite type relation |

规则：

- `p` partitioned table 表示 rsduck 托管分区表。DuckDB 执行层使用 generated view 实现查询入口。
- managed physical day table 是 `relkind = 'r'` 且 `relispartition = true`。
- `reloptions` 只放展示和低频配置，结构化生命周期字段必须放入 `rs_relation_ext` 或 `rs_partition`。

### 5.7 `rsduck_catalog.pg_attribute`

用途：记录 relation column。

主键：

- `(attrelid, attnum)`

唯一约束：

- `(attrelid, attname)`，排除 `attisdropped = true` 的历史列。

真实字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `attrelid` | BIGINT | relation OID。 |
| `attname` | VARCHAR | column 名称。 |
| `atttypid` | BIGINT | type OID。 |
| `attnum` | INT | column 顺序，从 1 开始。 |
| `atttypmod` | INT | 类型修饰，未使用为 -1。 |
| `attnotnull` | BOOLEAN | 是否 NOT NULL。 |
| `atthasdef` | BOOLEAN | 是否存在 default。 |
| `attisdropped` | BOOLEAN | 是否已删除。 |
| `attidentity` | VARCHAR | identity 标记，当前只展示。 |
| `attgenerated` | VARCHAR | generated column 标记，当前只展示。 |
| `attoptions` | VARCHAR | column options。 |

规则：

- `attnum` 一经分配不得因 DROP COLUMN 重新排序。
- DROP COLUMN 时优先标记 `attisdropped = true`，并同步执行 DuckDB DDL。
- physical partition table 的字段必须和 parent 分区表字段完全一致。

### 5.8 `rsduck_catalog.pg_attrdef`

用途：记录 default expression 和 generated expression。

主键：

- `oid`

唯一约束：

- `(adrelid, adnum)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `oid` | BIGINT | default object OID。 |
| `adrelid` | BIGINT | relation OID。 |
| `adnum` | INT | column attnum。 |
| `adbin` | VARCHAR | 标准化 expression。 |

规则：

- `adbin` 保存 rsduck 标准化表达式文本，不保存 PostgreSQL node tree。
- `pg_get_expr(adbin, adrelid)` 返回该表达式文本。
- DuckDB 不接受的 default expression 不得写入 metadata。

### 5.9 `rsduck_catalog.pg_constraint`

用途：记录 table constraint。

主键：

- `oid`

唯一约束：

- `(connamespace, conname)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `oid` | BIGINT | constraint OID。 |
| `conname` | VARCHAR | constraint 名称。 |
| `connamespace` | BIGINT | namespace OID。 |
| `contype` | VARCHAR | `p` primary key，`u` unique，`c` check，`f` foreign key。 |
| `conrelid` | BIGINT | table relation OID。 |
| `conindid` | BIGINT | backing index OID，没有则为 0。 |
| `conkey` | VARCHAR | column attnum 列表，例如 `1,2`。 |
| `confrelid` | BIGINT | foreign table relation OID，没有则为 0。 |
| `confkey` | VARCHAR | foreign column attnum 列表。 |
| `convalidated` | BOOLEAN | 是否已验证。 |
| `conbin` | VARCHAR | check expression 或标准化定义。 |

规则：

- constraint metadata 必须对应 DuckDB 已接受的约束或 rsduck 已明确执行的校验机制。
- 不允许创建 metadata-only constraint。
- 如果 DuckDB 拒绝约束 DDL，catalog mutation 必须整体失败。

### 5.10 `rsduck_catalog.pg_index`

用途：记录 index relation 和 table 的关系。

主键：

- `indexrelid`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `indexrelid` | BIGINT | index relation OID，指向 `pg_class.oid`。 |
| `indrelid` | BIGINT | table relation OID。 |
| `indnatts` | INT | index 字段总数。 |
| `indnkeyatts` | INT | key 字段数。 |
| `indisunique` | BOOLEAN | 是否 unique。 |
| `indisprimary` | BOOLEAN | 是否 primary backing index。 |
| `indisvalid` | BOOLEAN | 是否有效。 |
| `indkey` | VARCHAR | column attnum 列表。 |
| `indexprs` | VARCHAR | expression index 表达式，当前不支持则为空。 |
| `indpred` | VARCHAR | partial index predicate，当前不支持则为空。 |

规则：

- `pg_class` 中必须存在对应 `relkind = 'i'` 的 index relation。
- DuckDB 不接受的 index 不得写入 metadata。
- 不支持 expression index 和 partial index 时必须拒绝 DDL，不得写入空表达式伪装支持。

### 5.11 `rsduck_catalog.pg_depend`

用途：记录对象依赖。

主键：

- `(classid, objid, objsubid, refclassid, refobjid, refobjsubid)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `classid` | BIGINT | 依赖对象所在 catalog class OID。 |
| `objid` | BIGINT | 依赖对象 OID。 |
| `objsubid` | INT | 依赖对象子编号，relation 为 0，column 为 attnum。 |
| `refclassid` | BIGINT | 被依赖对象所在 catalog class OID。 |
| `refobjid` | BIGINT | 被依赖对象 OID。 |
| `refobjsubid` | INT | 被依赖对象子编号。 |
| `deptype` | VARCHAR | `n` normal，`a` auto，`i` internal。 |

规则：

- 分区表必须依赖所有 active physical partition table。
- index 必须依赖 table。
- constraint 必须依赖 table 和涉及 column。
- DROP 对象前必须检查 depend，除非 mutation 明确执行 cascade。

### 5.12 `rsduck_catalog.pg_description`

用途：记录 object comment。

主键：

- `(objoid, classoid, objsubid)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `objoid` | BIGINT | object OID。 |
| `classoid` | BIGINT | catalog class OID。 |
| `objsubid` | INT | column attnum，object 本身为 0。 |
| `description` | VARCHAR | comment 文本。 |

规则：

- `COMMENT ON` 只能作用于已存在对象。
- `obj_description` 和 `col_description` 从本表读取。

### 5.13 `rsduck_catalog.rs_relation_ext`

用途：保存 rsduck relation 私有属性。

主键：

- `relid`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `relid` | BIGINT | relation OID。 |
| `managed_kind` | VARCHAR | `ordinary` / `generated_view` / `range_partitioned_table` / `physical_partition`。 |
| `storage_mode` | VARCHAR | 当前固定为 `memory`。 |
| `visibility` | VARCHAR | `user` / `internal`。 |
| `partition_key` | VARCHAR | 分区字段名。 |
| `partition_key_type` | VARCHAR | `date` / `timestamp`。 |
| `partition_unit` | VARCHAR | `hour` / `day` / `month` / `year`。 |
| `retention_count` | INT | 保留最近 N 个 `partition_unit`，非分区对象为 0。 |
| `generated_sql` | VARCHAR | 分区表查询入口或 generated view 当前 SQL。 |
| `properties_json` | VARCHAR | 扩展属性 JSON。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `updated_at` | TIMESTAMP | 更新时间。 |

规则：

- `managed_kind = range_partitioned_table` 的 relation 必须是 `pg_class.relkind = 'p'`。
- `managed_kind = physical_partition` 的 relation 必须在 `rsduck_internal` schema。
- 用户可见对象 `visibility = user`，内部物理对象 `visibility = internal`。

### 5.14 `rsduck_catalog.rs_partition`

用途：记录 managed range partitioned table 的物理分片。

主键：

- `(parent_relid, child_relid)`

唯一约束：

- `(parent_relid, partition_value)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `parent_relid` | BIGINT | 分区表 relation OID。 |
| `child_relid` | BIGINT | physical table relation OID。 |
| `partition_value` | VARCHAR | 分区值。`hour=yyyyMMddHH`，`day=yyyyMMdd`，`month=yyyyMM`，`year=yyyy`。 |
| `partition_unit` | VARCHAR | `hour` / `day` / `month` / `year`。 |
| `lower_bound` | TIMESTAMP | 分区左边界。 |
| `upper_bound` | TIMESTAMP | 分区右边界。 |
| `is_null_partition` | BOOLEAN | 保留字段，当前版本固定为 `false`，不创建 null partition。 |
| `status` | VARCHAR | `creating` / `active` / `expiring` / `dropped` / `failed`。 |
| `row_count` | BIGINT | 记录行数。 |
| `min_ts` | TIMESTAMP | 最小时间。 |
| `max_ts` | TIMESTAMP | 最大时间。 |
| `checksum` | VARCHAR | 数据校验值。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `activated_at` | TIMESTAMP | 激活时间。 |
| `dropped_at` | TIMESTAMP | 删除时间。 |
| `error_message` | VARCHAR | 失败原因。 |

规则：

- 只有 `status = active` 的 partition 能进入分区表查询入口。
- `dropped` partition 可以保留历史行，但不得出现在 `pg_class` active relation 投影中。
- 创建或删除 partition 后必须重建分区表查询入口。
- range partitioned relation 不创建 null partition。
- 分区键必须声明 `NOT NULL`，分区键为空、无法解析或不可路由时写入失败。
- retention 只处理普通 active partitions，过期后必须重建分区表查询入口。

### 5.15 `rsduck_catalog.rs_user`

用途：记录 rsduck 登录账号。该表是真实认证来源，不是 PG role 兼容投影。

主键：

- `user_id`

唯一约束：

- `username`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `user_id` | BIGINT | 用户 ID，来自 OID allocator。 |
| `username` | VARCHAR | 登录名。 |
| `password_hash` | VARCHAR | 密码哈希，禁止保存明文。 |
| `password_algo` | VARCHAR | 哈希算法，例如 `argon2id`。 |
| `status` | VARCHAR | `active` / `disabled` / `locked`。 |
| `is_builtin` | BOOLEAN | 是否内置账号。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `updated_at` | TIMESTAMP | 更新时间。 |
| `last_login_at` | TIMESTAMP | 最近登录时间。 |

规则：

- PG wire 和 Web 登录都必须通过 `rs_user` 认证。
- `disabled` 和 `locked` 用户不得登录。
- 密码校验必须在 SQL 执行前完成。
- 内置 bootstrap admin 默认账号为 `admin`，默认密码为 `admin`。生产部署应由管理员自行修改默认密码。

### 5.16 `rsduck_catalog.rs_role`

用途：记录 rsduck 内部角色。角色是权限集合，不实现 PostgreSQL role inheritance。

主键：

- `role_id`

唯一约束：

- `role_name`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `role_id` | BIGINT | role ID。 |
| `role_name` | VARCHAR | 角色名。 |
| `description` | VARCHAR | 说明。 |
| `is_builtin` | BOOLEAN | 是否内置角色。 |
| `created_at` | TIMESTAMP | 创建时间。 |
| `updated_at` | TIMESTAMP | 更新时间。 |

内置角色：

| role | 语义 |
|------|------|
| `admin` | 全部权限，包括用户、权限、catalog 诊断和修复。 |
| `operator` | 运行维护权限，包括 snapshot、诊断、unavailable relation 修复，不包含普通用户管理。 |
| `ddl` | 用户对象 DDL 权限。 |
| `writer` | 数据写入权限。 |
| `reader` | 只读查询权限。 |

### 5.17 `rsduck_catalog.rs_user_role`

用途：记录用户和角色绑定。

主键：

- `(user_id, role_id)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `user_id` | BIGINT | 用户 ID。 |
| `role_id` | BIGINT | 角色 ID。 |
| `granted_by` | BIGINT | 授权人用户 ID。 |
| `created_at` | TIMESTAMP | 授权时间。 |

规则：

- 用户权限是其所有角色权限和显式对象权限的并集。
- 不支持角色继承角色。
- 不支持 PostgreSQL `SET ROLE` 语义。

### 5.18 `rsduck_catalog.rs_privilege`

用途：记录 schema、relation 和管理操作权限。

主键：

- `privilege_id`

唯一约束：

- `(principal_type, principal_id, object_type, object_id, action)`

字段：

| 字段 | 类型 | 语义 |
|------|------|------|
| `privilege_id` | BIGINT | 权限记录 ID。 |
| `principal_type` | VARCHAR | `user` / `role`。 |
| `principal_id` | BIGINT | 用户 ID 或 role ID。 |
| `object_type` | VARCHAR | `system` / `schema` / `relation`。 |
| `object_id` | BIGINT | object OID；system 权限为 0。 |
| `action` | VARCHAR | 权限动作。 |
| `granted_by` | BIGINT | 授权人用户 ID。 |
| `created_at` | TIMESTAMP | 授权时间。 |

权限动作：

| action | 适用对象 | 语义 |
|--------|----------|------|
| `read` | schema / relation | 允许 SELECT、DESCRIBE、metadata 展示。 |
| `write` | relation | 允许 INSERT、UPDATE、DELETE、COPY INTO。 |
| `ddl` | schema / relation | 允许 CREATE、ALTER、DROP、COMMENT、INDEX、CONSTRAINT mutation。 |
| `manage_snapshot` | system | 允许手动 snapshot、查看 snapshot 状态。 |
| `manage_catalog` | system | 允许 catalog 诊断、unavailable relation 修复。 |
| `manage_user` | system | 允许用户、角色、权限管理。 |

规则：

- 权限默认拒绝。
- `admin` 角色内置拥有所有 action。
- `operator` 角色内置拥有 `manage_snapshot`、`manage_catalog` 和诊断只读权限。
- `reader`、`writer`、`ddl` 的具体对象范围必须通过 `rs_privilege` 授予。
- reserved schema 权限不能授予普通角色。只有 `admin` 和 `operator` 可按诊断规则读取内部状态。

## 6. 类型映射

managed DDL 只接受以下 DuckDB 类型，并映射到固定 PG type OID：

| DuckDB 类型 | PG 类型 | PG OID |
|-------------|---------|--------|
| `BOOLEAN` | `bool` | 16 |
| `SMALLINT` | `int2` | 21 |
| `INTEGER` | `int4` | 23 |
| `BIGINT` | `int8` | 20 |
| `REAL` | `float4` | 700 |
| `DOUBLE` / `DOUBLE PRECISION` | `float8` | 701 |
| `DECIMAL` / `NUMERIC` | `numeric` | 1700 |
| `VARCHAR` | `varchar` | 1043 |
| `TEXT` | `text` | 25 |
| `DATE` | `date` | 1082 |
| `TIME` | `time` | 1083 |
| `TIMESTAMP` | `timestamp` | 1114 |

规则：

- 类型名在进入 catalog 前必须标准化。
- 不在表中的类型必须拒绝，错误信息包含原始 DuckDB 类型名。
- `format_type(oid, typmod)` 只对表中类型和 relation composite type 返回结果。
- composite row type 的 OID 由 `rs_oid_alloc` 分配，不能复用 relation OID。

## 7. Relation 表达

### 7.1 普通表

普通用户表表达：

```text
pg_class:
  relkind = 'r'
  relispartition = false

rs_relation_ext:
  managed_kind = 'ordinary'
  visibility = 'user'
```

DuckDB 中必须存在同名 table。

### 7.2 普通视图

普通用户视图表达：

```text
pg_class:
  relkind = 'v'
  relispartition = false

rs_relation_ext:
  managed_kind = 'generated_view'
  visibility = 'user'
```

DuckDB 中必须存在同名 view。`pg_depend` 必须记录视图依赖。

### 7.3 分区表

用户 DDL：

```sql
CREATE TABLE ods_access_log (
    id BIGINT,
    user_id VARCHAR(64),
    access_time TIMESTAMP,
    content TEXT
)
PARTITION BY RANGE (access_time)
WITH (
    partition_unit = 'day',
    retention = '30'
);
```

用户视角：

```text
main.ods_access_log 是分区表。
用户按表查询、授权、注释和管理，不直接感知内部物理分区表。
```

catalog 表达：

```text
pg_class:
  main.ods_access_log                       relkind = 'p', relispartition = false
  rsduck_internal.ods_access_log_20260701   relkind = 'r', relispartition = true
  rsduck_internal.ods_access_log_20260702   relkind = 'r', relispartition = true
  rsduck_internal.ods_access_log_null       relkind = 'r', relispartition = true

rs_relation_ext:
  main.ods_access_log                       managed_kind = 'range_partitioned_table'
  rsduck_internal.ods_access_log_20260701   managed_kind = 'physical_partition'

rs_partition:
  parent_relid = ods_access_log oid
  child_relid = ods_access_log_20260701 oid
  partition_value = '20260701'
  partition_unit = 'day'
  is_null_partition = false
  status = 'active'

pg_depend:
  ods_access_log partitioned table -> active physical partition tables
```

DuckDB 当前没有通用原生分区表。rsduck 对外把该对象定义为分区表；在 DuckDB 执行层使用 generated view 汇总 active physical partitions。分区按 `partition_value` 升序生成。下面的 SQL 是内部生成结果，不是用户 DDL。

有 active partitions：

```sql
CREATE OR REPLACE VIEW main.ods_access_log AS
SELECT * FROM rsduck_internal.ods_access_log_20260701
UNION ALL
SELECT * FROM rsduck_internal.ods_access_log_20260702;
```

无 active partitions 时，分区表查询入口仍必须存在，使用 typed zero-row view 保持 schema 可见：

```sql
CREATE OR REPLACE VIEW main.ods_access_log AS
SELECT
  CAST(NULL AS BIGINT) AS id,
  CAST(NULL AS VARCHAR) AS user_id,
  CAST(NULL AS TIMESTAMP) AS access_time,
  CAST(NULL AS TEXT) AS content
WHERE FALSE;
```

### 7.4 Range 分区校验规则

rsduck 只支持受控 range 分区：

```text
PARTITION BY RANGE (column_name)
WITH (
    partition_unit = 'hour|day|month|year',
    retention = 'positive_integer'
)
```

规则：

- `PARTITION BY RANGE` 只支持单列，不支持表达式。
- 分区列只允许 `DATE` 或 `TIMESTAMP`。
- 分区列必须声明 `NOT NULL`，分区键为空、无法解析或不可路由时写入失败。
- `partition_unit` 必填，只允许 `hour`、`day`、`month`、`year`。
- `retention` 必填，必须是正整数文本。
- `retention = N` 表示保留最近 N 个 `partition_unit`。
- 物理分区由 rsduck 自动创建、过期、DROP 和重建 view，普通用户不得手工维护。

类型和单位兼容矩阵：

| 分区列类型 | 允许 partition_unit | 说明 |
|------------|---------------------|------|
| `TIMESTAMP` | `hour`, `day`, `month`, `year` | 按时间戳截断到对应粒度。 |
| `DATE` | `day`, `month`, `year` | DATE 没有小时精度，禁止 `hour`。 |

分区值格式：

| partition_unit | partition_value | 物理表后缀 |
|----------------|-----------------|------------|
| `hour` | `yyyyMMddHH` | `2026070513` |
| `day` | `yyyyMMdd` | `20260705` |
| `month` | `yyyyMM` | `202607` |
| `year` | `yyyy` | `2026` |

不可路由数据处理：

- 分区键为 `NULL` 的行写入失败。
- 结构化写入 API 中，分区键原始值无法转换成 `DATE` 或 `TIMESTAMP` 时写入失败。
- 分区键无法按 `partition_unit` 计算边界时写入失败。
- rsduck 不创建 null partition，不隔离保存脏分区键数据；调用方必须先清洗数据或修正 SQL。

## 8. 账号与权限模型

rsduck 必须实现最小真实权限模型。PG catalog 中的 `relowner`、`nspowner`、ACL 字段和权限函数只做兼容展示；真实认证和授权必须来自 `rsduck_catalog.rs_user`、`rs_role`、`rs_user_role`、`rs_privilege`。

### 8.1 认证

PG wire 和 Web 都必须在建立 session 时完成认证：

```text
username + password/token
  -> rs_user lookup
  -> password hash verify
  -> status check
  -> load roles and privileges
  -> create session principal
```

规则：

- 未认证 session 不得执行 SQL。
- 认证失败必须返回统一错误，不泄露用户是否存在。
- session principal 必须绑定 `user_id`、`username`、roles、system privileges。
- 密码 hash 算法必须可版本化，便于后续升级。

### 8.2 Bootstrap 默认账号

首次 catalog bootstrap 必须创建默认管理员：

```text
username = admin
password = admin
role = admin
status = active
is_builtin = true
```

写入规则：

```text
rs_user:
  username = 'admin'
  password_hash = hash('admin')
  password_algo = 'argon2id'
  status = 'active'
  is_builtin = true

rs_role:
  role_name = 'admin'
  is_builtin = true

rs_user_role:
  admin user -> admin role
```

规则：

- 默认密码不为空，不使用无密码登录。
- rsduck 不强制首次登录修改密码。
- 管理员应在生产部署后通过管理命令或 Web 控制台自行修改默认密码。
- 默认管理员账号不得被删除；如需禁用，必须先确保存在另一个 active admin 用户。

### 8.3 密码修改与离线重置

密码修改分两类：正常在线修改和忘记 `admin` 密码后的离线重置。

#### 8.3.1 正常在线修改

管理员已登录时，使用标准 catalog mutation：

```sql
ALTER USER admin PASSWORD 'new_password';
```

执行规则：

1. SQL router 将该语句归类为 write。
2. 请求进入 single write worker。
3. `catalog::mutation::user_role` 校验调用者拥有 `manage_user` 权限。
4. 使用当前密码 hash 算法生成新 `password_hash`。
5. 在 `run_catalog_tx` 中写入 `rs_catalog_journal pending`。
6. 更新 `rsduck_catalog.rs_user.password_hash` 和 `password_algo`。
7. `finish_journal` 将 journal 标记为 applied。
8. 推进 `catalog_epoch` 并刷新 `catalog_checksum`。

禁止事项：

- 禁止直接 `UPDATE rsduck_catalog.rs_user`。
- 禁止把明文密码写入 `rsduck.toml`。
- 禁止保留旧密码兼容字段或临时 fallback 认证路径。

#### 8.3.2 忘记 admin 密码时的离线重置

当 `admin` 密码遗忘且没有其他 active admin 用户时，目标产品能力是离线维护命令：

```powershell
rsduck reset-admin-password
rsduck reset-admin-password --password admin123
```

不传 `--password` 时固定把内置 `admin` 用户密码重置为 `admin`；传入 `--password` 时重置为指定密码。它是离线工具能力，不启动 rsduck 服务，不监听 PG/Web 端口，不启动 read/write/snapshot workers。它只在当前进程内打开临时 DuckDB connection。

执行前置条件：

- rsduck 正常服务必须已停止。
- 命令必须先尝试获取 rsduck 进程独占锁。
- 进程锁被占用时必须失败，不得继续操作 snapshot。
- `.rsduck.lock` 可获取但文件已存在时，视为 stale lock；命令可读取 PID 诊断信息后覆盖。

`.rsduck.lock` 保存 JSON 诊断信息：

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

判断服务是否仍在运行必须依赖文件独占锁；PID 只用于错误提示和人工诊断，不能作为唯一判断依据。

离线重置流程：

1. 读取当前目录 `rsduck.toml`。
2. 定位 `snapshot.dir` 下最新正式 snapshot 目录。
3. 拒绝使用 `.tmp` snapshot 目录。
4. 打开临时 DuckDB connection。
5. `IMPORT DATABASE '<latest_snapshot>'`。
6. 校验 `rsduck_snapshot_manifest.json` 和 catalog checksum。
7. 调用受控 catalog mutation 等价逻辑执行 `ALTER USER admin PASSWORD '<new_password>'`。
8. `EXPORT DATABASE` 到新的 `.tmp` snapshot 目录。
9. 写入新的 snapshot manifest。
10. 原子 rename 为新的正式 snapshot 目录。
11. 保留原 snapshot，不覆盖、不原地修改。

禁止事项：

- 禁止直接修改 `rsduck_catalog_rs_user.parquet`。
- 禁止只修改 `rs_user` 而不刷新 `rs_catalog_version.catalog_checksum`。
- 禁止手写 manifest checksum。
- 禁止在 rsduck 服务仍持锁运行时执行重置。
- 禁止重置不存在的用户；当前离线重置只面向内置 `admin`。

失败处理：

- 导入 snapshot 失败：不生成新 snapshot。
- manifest 或 checksum 校验失败：拒绝重置。
- 写 `.tmp` snapshot 失败：删除本次 `.tmp` 目录，保留原 snapshot。
- rename 失败：保留原 snapshot，并返回明确错误。

### 8.4 授权动作

SQL router 在执行前必须把请求归类成 action：

| SQL / 操作 | 所需权限 |
|------------|----------|
| `SELECT` 用户 relation | relation 或所在 schema 的 `read`。 |
| `DESCRIBE` / metadata 展示 | relation 或所在 schema 的 `read`。 |
| `INSERT` / `UPDATE` / `DELETE` | relation 的 `write`。 |
| `COPY INTO` | relation 的 `write`。 |
| `CREATE TABLE` / `CREATE VIEW` | schema 的 `ddl`。 |
| `ALTER` / `DROP` / `COMMENT` | relation 的 `ddl`。 |
| `CREATE INDEX` / `DROP INDEX` | relation 的 `ddl`。 |
| 手动 snapshot | system `manage_snapshot`。 |
| catalog 诊断 | system `manage_catalog`。 |
| unavailable relation 修复 | system `manage_catalog`。 |
| 用户、角色、权限管理 | system `manage_user`。 |

规则：

- 权限默认拒绝。
- `admin` 拥有所有权限。
- `operator` 只能做运行维护和诊断，不自动拥有用户数据写入和 DDL 权限。
- 对 managed physical partition table 的直接读写默认拒绝，即使用户拥有 parent 分区表权限。
- 分区表查询入口的权限继承自 parent relation，不继承 physical partition table。

### 8.5 Reserved Schema 权限

reserved schema 权限规则：

| schema | 默认行为 |
|--------|----------|
| `pg_catalog` | 允许认证用户只读查询兼容投影。 |
| `information_schema` | 允许认证用户只读查询兼容投影。 |
| `rsduck_catalog` | 默认拒绝；`admin` / `operator` 诊断模式可只读。 |
| `rsduck_internal` | 默认拒绝；`admin` / `operator` 诊断模式可只读。 |

规则：

- `rsduck_catalog` 和 `rsduck_internal` 不接受普通 `read/write/ddl` 授权。
- 内部 mutation planner 不通过用户 SQL 权限绕行，而是使用 internal execution context。
- 诊断模式查询必须写审计日志。

### 8.6 PG 兼容投影

PG 兼容对象必须反映 rsduck session 用户，但不实现完整 PG ACL：

- `current_user` / `session_user` 返回当前 rsduck username。
- `pg_get_userbyid(oid)` 对已知 rsduck user 返回 username；未知 owner 返回 `unknown`。
- `has_database_privilege`、`has_schema_privilege`、`has_table_privilege` 根据 `rs_privilege` 计算。
- `pg_roles` 和 `pg_user` 从 `rs_user` / `rs_role` 派生兼容行。
- `relowner`、`nspowner` 当前只用于兼容展示，不作为授权判断来源。

### 8.7 审计要求

以下操作必须记录审计事件：

- 登录成功和失败。
- 权限拒绝。
- 用户、角色、权限变更。
- snapshot / restore。
- catalog 诊断查询。
- unavailable relation 修复。
- reserved schema 诊断访问。

审计事件可以先写入 rsduck 日志，后续需要持久化时再增加 `rsduck_catalog.rs_audit_log`。

## 9. Catalog Mutation Contract

所有 catalog 变更必须进入 `CatalogMutation` 内部流程。SQL DDL 和 management API 只是入口不同，最终都必须走同一个 mutation planner。

通用流程：

```text
1. normalize request
2. validate catalog contract
3. acquire catalog write lock through single write worker
4. BEGIN DuckDB transaction
5. insert rs_catalog_journal status = pending
6. mutate rsduck_catalog.*
7. execute DuckDB physical DDL
8. run mutation-local consistency checks
9. update rs_catalog_journal status = applied
10. increment rs_catalog_version.catalog_epoch
11. COMMIT
```

失败行为：

- 任一步失败必须 rollback。
- rollback 成功后不得留下 active catalog row。
- 无法 rollback 的外部副作用必须在恢复校验中被识别，并使启动失败或进入明确修复流程。
- 错误信息必须包含 mutation type、目标对象和失败步骤。

分区维护执行边界：

- 分区维护不是独立写路径，必须复用本章 catalog mutation contract。
- timer 触发的维护任务由 partition scheduler 投递到 write queue，再由 single write worker 执行。
- manual 触发的维护任务由 SQL/API 鉴权后投递到 write queue，再由 single write worker 执行。
- write trigger 触发的分区创建发生在 write worker 的当前写入流程中，必须和本次写入保持顺序一致。
- partition scheduler 不得持有 DuckDB connection，不得直接执行 DuckDB DDL，不得直接修改 `rsduck_catalog.*`。
- 分区维护 mutation 必须写 `rs_catalog_journal`，并递增 `catalog_epoch`。

分区维护 job 和 mutation 的对应关系：

| Job | Catalog mutation |
|-----|------------------|
| `EnsurePartitionedTable` | `refresh_partition_entrypoint` / consistency check |
| `CreateRangePartition` | `create_range_partition` |
| `ExpirePartition` | `expire_partition` |
| `RefreshPartitionEntrypoint` | `refresh_partition_entrypoint` |
| `VerifyPartitionManifest` | startup / runtime consistency check |
| `MarkPartitionUnavailable` | `mark_partition_unavailable` |

### 9.1 `create_schema`

输入：

```text
schema_name
owner
```

步骤：

```text
1. 校验 schema_name 不是保留 schema。
2. 校验 pg_namespace 中不存在同名 schema。
3. 分配 namespace oid。
4. 写 pg_namespace。
5. 执行 DuckDB CREATE SCHEMA。
6. 写 journal applied。
```

### 9.2 `create_table`

输入：

```text
schema_name
table_name
columns
constraints
options
```

步骤：

```text
1. 校验 schema 存在且不是保留 schema。
2. 校验 relation 名称不冲突。
3. 校验 column 名称、类型、nullable、default。
4. 校验 constraint 可由 DuckDB 接受。
5. 分配 relation oid 和 composite type oid。
6. 写 pg_type composite row type。
7. 写 pg_class relkind = 'r'。
8. 写 pg_attribute。
9. 写 pg_attrdef。
10. 写 pg_constraint。
11. 写 rs_relation_ext managed_kind = 'ordinary'。
12. 执行 DuckDB CREATE TABLE。
13. 写 dependency。
14. 写 journal applied。
```

### 9.3 `create_range_partitioned_table`

输入：

```text
schema_name
logical_name
columns
partition_key
partition_unit
retention
```

步骤：

```text
1. 校验 PARTITION BY RANGE 只包含单个 column。
2. 校验 partition_key 是 columns 中的 DATE 或 TIMESTAMP 字段。
3. 校验 partition_key 必须声明 NOT NULL。
4. 校验 partition_unit 和 partition_key 类型兼容。
5. 校验 retention 是正整数。
6. 分配 partitioned table relid 和 composite type oid。
7. 写 pg_type composite row type。
8. 写 pg_class relkind = 'p'。
9. 写 pg_attribute。
10. 写 rs_relation_ext managed_kind = 'range_partitioned_table'。
11. 创建 DuckDB generated view 作为空查询入口。
12. 写 journal applied。
```

### 9.4 `create_range_partition`

输入：

```text
parent_relid
partition_value
```

步骤：

```text
1. 校验 parent relation 存在且 managed_kind = 'range_partitioned_table'。
2. 根据 partition_key、partition_unit 标准化 partition_value 和上下边界。
3. 如果同 parent_relid + partition_value 已经 active，直接返回该 partition。
4. 如果存在 creating / failed / dropped 状态，返回明确错误，要求人工修复或显式 retry mutation。
5. 生成 physical table name。
6. 分配 child relid 和 composite type oid。
7. 写 pg_type composite row type。
8. 写 pg_class relkind = 'r', relispartition = true。
9. 复制 parent pg_attribute 到 child。
10. 写 rs_relation_ext managed_kind = 'physical_partition', visibility = 'internal'。
11. 写 rs_partition status = 'creating'。
12. 执行 DuckDB CREATE TABLE rsduck_internal.{name}。
13. 更新 rs_partition status = 'active'。
14. 调用 refresh_partition_entrypoint。
15. 写 journal applied。
```

### 9.5 `refresh_partition_entrypoint`

输入：

```text
parent_relid
```

步骤：

```text
1. 读取 parent columns。
2. 读取 active partitions，按 partition_value 升序；如果没有 active partitions，使用 parent columns 生成 typed zero-row view。
3. 生成 deterministic entrypoint SQL。
4. 执行 DuckDB CREATE OR REPLACE VIEW。
5. 删除 parent 分区表旧 pg_depend。
6. 写 parent 分区表到 active child table 的 pg_depend。
7. 更新 rs_relation_ext.generated_sql。
```

规则：

- 该 mutation 必须幂等。
- 生成 SQL 必须稳定，便于 checksum 和测试。
- 该 mutation 只能改变分区表查询入口和 dependency，不得创建或删除 physical partition table。

### 9.6 `expire_partition`

输入：

```text
parent_relid
partition_value
```

步骤：

```text
1. 校验 partition 当前 status = active。
2. 更新 rs_partition status = expiring。
3. 调用 refresh_partition_entrypoint，先让分区表查询入口移除该 child。
4. 执行 DuckDB DROP TABLE rsduck_internal.{physical_table}。
5. 删除 child relation 的 active pg_class / pg_attribute / pg_type / pg_depend / pg_description。
6. 更新 rs_partition status = dropped, dropped_at = now。
7. 写 journal applied。
```

规则：

- 过期后，普通 catalog 投影不得再显示 dropped physical relation。
- `rs_partition` 可以保留 dropped 历史，用于审计和排查。
- retention 只过期 retention 窗口外的 active partitions。

### 9.7 `drop_relation`

输入：

```text
relid
cascade
```

步骤：

```text
1. 检查 pg_depend。
2. 如果存在 dependent object 且 cascade = false，返回错误。
3. 如果 cascade = true，按依赖拓扑排序删除。
4. 删除 DuckDB physical object。
5. 删除或标记 catalog rows。
6. 写 journal applied。
```

规则：

- 禁止直接 drop active physical partition；必须通过 `expire_partition`。
- 禁止普通用户 drop 保留 schema 下对象。

### 9.8 `alter_table_add_column`

输入：

```text
relid
column_definition
```

步骤：

```text
1. 校验 relid 是 ordinary table 或 range_partitioned_table。
2. 分配新 attnum = max(attnum) + 1。
3. 校验类型和 default。
4. 对 ordinary table 执行 DuckDB ALTER TABLE ADD COLUMN。
5. 对 range_partitioned_table，必须对所有 active physical partitions 执行 ALTER TABLE ADD COLUMN。
6. 写 parent / child pg_attribute。
7. 如有 default，写 pg_attrdef。
8. 对 range_partitioned_table 调用 refresh_partition_entrypoint。
9. 写 journal applied。
```

规则：

- 如果任何 active physical partition 修改失败，整个 mutation 必须失败。
- 不允许只修改 parent 分区表 metadata。

### 9.9 `create_index`

输入：

```text
table_relid
index_name
columns
unique
```

步骤：

```text
1. 校验 table_relid 是 ordinary table。
2. 校验 columns 存在。
3. 拒绝 expression index 和 partial index。
4. 分配 index relid。
5. 写 pg_class relkind = 'i'。
6. 写 pg_index。
7. 执行 DuckDB CREATE INDEX。
8. 更新 table pg_class.relhasindex。
9. 写 pg_depend。
10. 写 journal applied。
```

规则：

- managed physical partition table 的 index 管理必须由 parent relation 的专门 mutation 驱动，不允许用户单独创建。

### 9.10 `comment_object`

输入：

```text
object_identity
description
```

步骤：

```text
1. 解析 object_identity。
2. 校验对象存在。
3. upsert pg_description。
4. 写 journal applied。
```

### 9.11 `mark_partition_unavailable`

输入：

```text
parent_relid
child_relid
reason
```

步骤：

```text
1. 校验 parent relation 是 range_partitioned_table。
2. 校验 child relation 是 parent 的 physical partition。
3. 更新 rs_partition status = failed 或 unavailable。
4. 记录 error_message。
5. 调用 refresh_partition_entrypoint，移除不可用普通分区。
6. 写 journal applied。
```

规则：

- 该 mutation 只隔离异常对象，不删除 physical table。
- 修复必须通过显式 repair 或重建 mutation 完成。

### 9.12 脏分区键处理

当前版本不提供 `cleanup_null_partition`，也不创建 null partition。分区表创建时要求分区键 `NOT NULL`，写入时如果分区键为空、无法转换或不可路由，当前写入直接失败。

规则：

- 分区键问题由调用方在写入前清洗或修正。
- rsduck 不保留脏分区键数据的内部隔离区。
- retention 和 repair 只处理真实存在的 physical partitions。

## 10. 对外兼容查询矩阵

### 10.1 支持的 `pg_catalog` relation

| relation | 行为 |
|----------|------|
| `pg_catalog.pg_namespace` | 从 `rsduck_catalog.pg_namespace` 投影。 |
| `pg_catalog.pg_class` | 投影 active relation，包含 table/view/index/partitioned table。 |
| `pg_catalog.pg_attribute` | 投影 active relation columns，不返回 dropped columns，除非查询显式要求。 |
| `pg_catalog.pg_type` | 投影 built-in type 和 composite row type。 |
| `pg_catalog.pg_constraint` | 投影 active constraint。 |
| `pg_catalog.pg_index` | 投影 active index。 |
| `pg_catalog.pg_attrdef` | 投影 active default expression。 |
| `pg_catalog.pg_depend` | 投影 active dependency。 |
| `pg_catalog.pg_description` | 投影 comment。 |
| `pg_catalog.pg_tables` | 从 `pg_class` / `pg_namespace` 派生。 |
| `pg_catalog.pg_views` | 从 `pg_class` / `rs_relation_ext` 派生。 |
| `pg_catalog.pg_indexes` | 从 `pg_index` / `pg_class` 派生。 |
| `pg_catalog.pg_database` | 返回当前单 database 兼容行。 |
| `pg_catalog.pg_roles` | 从 `rs_role` 派生兼容行。 |
| `pg_catalog.pg_user` | 从 `rs_user` 派生兼容行。 |
| `pg_catalog.pg_settings` | 返回 rsduck 支持的 session setting。 |

### 10.2 明确空结果的 `pg_catalog` relation

这些对象类别 rsduck 没有产品语义，但常见工具会探测。它们必须返回合法列结构和空结果：

| relation | 行为 |
|----------|------|
| `pg_catalog.pg_trigger` | 空结果。 |
| `pg_catalog.pg_proc` | 只在函数投影需要时返回内置兼容函数，否则空结果。 |
| `pg_catalog.pg_extension` | 空结果。 |
| `pg_catalog.pg_policy` | 空结果。 |
| `pg_catalog.pg_matviews` | 没有 materialized view 时空结果。 |
| `pg_catalog.pg_sequences` | 空结果。 |

未列入 10.1 或 10.2 的 `pg_catalog` relation 查询必须返回明确错误：

```text
unsupported pg_catalog relation: {name}
```

### 10.3 支持的 `information_schema` relation

| relation | 行为 |
|----------|------|
| `information_schema.schemata` | 从 `pg_namespace` 派生。 |
| `information_schema.tables` | 从 `pg_class` 派生 table/view。 |
| `information_schema.columns` | 从 `pg_attribute` / `pg_type` 派生。 |
| `information_schema.views` | 从 `pg_class` / `rs_relation_ext.generated_sql` 派生。 |
| `information_schema.table_constraints` | 从 `pg_constraint` 派生。 |
| `information_schema.key_column_usage` | 从 `pg_constraint.conkey` 派生。 |
| `information_schema.constraint_column_usage` | 从 `pg_constraint` 派生。 |

### 10.4 支持的兼容函数

| 函数 | 行为 |
|------|------|
| `version()` | 返回 rsduck PG wire adapter version。 |
| `current_database()` | 返回固定 database 名。 |
| `current_schema()` | 返回当前 schema，默认 `main`。 |
| `current_user` / `session_user` | 返回当前 rsduck session username。 |
| `current_setting(name)` | 返回支持的 setting，不支持则错误。 |
| `format_type(oid, typmod)` | 返回 type display name。 |
| `pg_table_is_visible(oid)` | 根据当前 schema 和 relation namespace 判断。 |
| `pg_get_expr(expr, relid)` | 返回标准化 expression 文本。 |
| `pg_get_constraintdef(oid)` | 从 `pg_constraint` 生成标准化 constraint definition。 |
| `obj_description(oid)` | 从 `pg_description` 读取。 |
| `col_description(oid, attnum)` | 从 `pg_description` 读取。 |
| `pg_get_userbyid(oid)` | 从 `rs_user` 或兼容 owner 映射返回用户名。 |
| `has_database_privilege(...)` | 根据当前 session 和 `rs_privilege` 返回结果。 |
| `has_schema_privilege(...)` | 根据当前 session 和 `rs_privilege` 返回结果。 |
| `has_table_privilege(...)` | 根据当前 session 和 `rs_privilege` 返回结果。 |

权限函数是兼容投影，但结果必须来自 rsduck 真实权限模型。

## 11. Reserved Schema 访问规则

SQL router 必须拦截 reserved schema：

| 操作 | 行为 |
|------|------|
| `SELECT pg_catalog.*` | 认证用户只允许走 catalog projection。 |
| `SELECT information_schema.*` | 认证用户只允许走 catalog projection。 |
| `SELECT rsduck_catalog.*` | 默认拒绝；`admin` / `operator` 诊断模式可开放只读。 |
| `SELECT rsduck_internal.*` | 默认拒绝；`admin` / `operator` 诊断模式可开放只读。 |
| `INSERT/UPDATE/DELETE rsduck_catalog.*` | 拒绝。 |
| `DDL pg_catalog.*` | 拒绝。 |
| `DDL information_schema.*` | 拒绝。 |
| `DDL rsduck_catalog.*` | 拒绝。 |
| `DDL rsduck_internal.*` | 只允许 internal mutation planner 执行。 |

错误必须明确：

```text
reserved schema is managed by rsduck catalog: {schema}
```

## 12. 启动恢复和一致性校验

启动流程：

```text
1. 打开新的 in-memory DuckDB。
2. 如果配置启用 snapshot restore，IMPORT DATABASE 最新正式快照。
3. 检查 rsduck_catalog.rs_catalog_version。
4. 如果 catalog 不存在且数据库没有用户对象，执行 catalog bootstrap。
5. 如果 catalog 不存在但数据库存在用户对象，启动失败。
6. 加载 rsduck_catalog.*。
7. 检查 rs_catalog_journal 中 pending / failed mutation。
8. 校验 active catalog relation 和 DuckDB physical objects 一致，区分全局级错误和对象级错误。
9. 根据 rs_partition 重建可用分区表的查询入口。
10. 计算 catalog checksum 并比对 rs_catalog_version.catalog_checksum。
11. 全局校验通过后设置 status = ready，并输出对象级告警摘要。
12. 启动 PG wire 和 Web 服务。
```

不得执行的行为：

- 不得在 catalog 缺失时扫描 DuckDB 用户表并自动生成 catalog。
- 不得在全局 catalog 恢复失败时启动空库。
- 不得静默忽略 view 重建失败；对象级失败必须记录告警并隔离相关对象。
- 不得自动尝试更早 snapshot。

一致性校验规则：

| 检查项 | 处理行为 |
|--------|----------|
| `rsduck_catalog` 系统表缺失或版本不受支持。 | 启动失败。 |
| `pg_class.relnamespace` 必须存在。 | 启动失败。 |
| `pg_attribute.attrelid` 必须存在。 | 启动失败。 |
| `pg_attribute.atttypid` 必须存在。 | 启动失败。 |
| namespace 内 active relation 名称不得重复。 | 启动失败。 |
| `pg_depend` 引用不存在的 catalog object。 | 启动失败。 |
| active table/view/index 缺失对应 DuckDB physical object。 | 输出告警，标记该 relation 为 unavailable，服务继续启动。 |
| DuckDB physical object column 顺序和 active `pg_attribute` 不一致。 | 输出告警，标记该 relation 为 unavailable，服务继续启动。 |
| active partition 的 child physical table 缺失或不可用。 | 输出告警，标记该 partition 和 parent 分区表为 unavailable，服务继续启动。 |
| 分区表查询入口 SQL 不等于 active partitions 生成结果。 | 尝试重建；重建失败则输出告警，标记该分区表为 unavailable，服务继续启动。 |

对象级 unavailable 规则：

- unavailable 只表示单个 relation 当前不可安全查询，不代表整个 rsduck 服务不可用。
- 对 unavailable relation 的普通查询必须返回明确错误，错误信息包含 schema、relation、原因和启动告警编号。
- catalog 投影仍可展示 unavailable relation，但必须提供诊断字段或诊断接口，让运维能看到异常原因。
- rsduck 不得为了启动成功而静默删除 catalog row、静默 DROP DuckDB 对象、或把分区表查询入口改写成只包含部分数据的 view。
- 修复必须通过显式管理操作完成，例如重新导入物理表、重建 view、删除损坏 relation 或重跑对应 mutation。

## 13. Snapshot 和 Catalog 的关系

rsduck snapshot 保存完整 DuckDB database，因此包含：

- 用户表和视图。
- `rsduck_catalog.*` 内部表。
- `rsduck_internal.*` 物理分片表。
- `pg_catalog.*` / `information_schema.*` 物理投影如果实现为 DuckDB view，也会随 snapshot 保存。

规则：

- snapshot restore 后仍必须跑 catalog consistency check。
- snapshot 不能替代 catalog journal。
- snapshot 成功后必须记录对应 `catalog_epoch` 和 `catalog_checksum`。
- 如果 snapshot 中存在 catalog 表，但版本不受支持，启动失败。

## 14. Range Partitioned Table 规范

非分区表：

```text
对象类型：ordinary table
schema：main 或业务指定 schema
生命周期：长期保留
retention：0
generated view：无
```

分区表：

```text
对象类型：range_partitioned_table
partitioned relation：main.ods_access_log
physical schema：rsduck_internal
partition key：access_time
partition unit：hour / day / month / year
retention：最近 N 个 partition_unit
查询入口：main.ods_access_log
```

写入路径：

```text
append_batch(relation, rows)
  -> validate partition key
  -> reject invalid or null partition key rows
  -> create_range_partition if missing
  -> append rows into physical partition
  -> update rs_partition row_count / min_ts / max_ts / checksum
```

查询路径：

```sql
SELECT *
FROM main.ods_access_log
WHERE access_time >= TIMESTAMP '2026-07-01 00:00:00'
ORDER BY access_time;
```

业务查询不得依赖 physical table 名称。分区键为 `NULL` 或无法路由的数据不会进入分区表，写入阶段会返回错误。

## 15. 验收测试

catalog 实现必须通过以下测试：

### 15.1 基础 catalog

- 创建 schema 后，`pg_catalog.pg_namespace` 和 `information_schema.schemata` 能查到。
- 创建 table 后，`pg_catalog.pg_class` 能查到 `relkind = 'r'`。
- 创建 table 后，`pg_catalog.pg_attribute` 字段顺序和 DDL 一致。
- 创建 table 后，`information_schema.columns` 返回正确 type、nullable、default。
- 创建 view 后，`pg_catalog.pg_class` 能查到 `relkind = 'v'`。
- 创建 index 后，`pg_catalog.pg_class` 和 `pg_catalog.pg_index` 一致。
- 创建 constraint 后，`pg_catalog.pg_constraint` 和 `information_schema.table_constraints` 一致。
- `format_type`、`pg_get_expr`、`pg_get_constraintdef` 返回稳定文本。

### 15.2 Managed range partitioned table

- 创建 `ods_access_log` 后，不创建 physical partition；分区表查询入口是 typed zero-row view。
- 创建两个 day partition 后，`rs_partition` 有两个 active 分区。
- 创建两个 day partition 后，分区表查询入口 SQL 按 `partition_value` 升序 `UNION ALL`。
- `DATE` 分区列使用 `partition_unit = 'hour'` 必须失败。
- `TIMESTAMP` 分区列支持 `hour`、`day`、`month`、`year`。
- 分区键为 `NULL` 或无法转换的行必须写入失败。
- `pg_depend` 记录分区表到 active physical tables 的依赖。
- 过期一个 partition 后，分区表查询入口移除该 physical table。
- 过期一个 partition 后，普通 `pg_catalog.pg_class` 不再显示 dropped physical relation。
- `rs_partition` 保留 dropped 历史和 `dropped_at`。

### 15.3 恢复

- snapshot restore 后 catalog checksum 一致。
- snapshot restore 后分区表查询入口自动重建。
- 手工破坏单个 DuckDB physical table 后服务仍可启动，并输出该 relation unavailable 告警。
- 手工破坏单个 DuckDB physical table 的字段顺序后服务仍可启动，并输出该 relation unavailable 告警。
- 查询 unavailable relation 时返回明确错误，不影响其他 relation 查询。
- 存在 pending journal 时启动执行恢复校验，不能静默忽略。
- catalog 缺失但存在用户对象时启动失败。

### 15.4 客户端兼容

- `psql` 可连接并查询 `\dt` 所需 catalog。
- DBeaver 可展开 schema、table、columns。
- Navicat 可展开 database、schema、table、columns。
- 常见 ORM 可读取 table 和 column metadata。
- 未支持的 `pg_catalog` relation 返回明确错误或定义好的空结果。

### 15.5 Reserved schema

- 外部 `INSERT INTO rsduck_catalog.*` 被拒绝。
- 外部 `CREATE TABLE pg_catalog.*` 被拒绝。
- 外部直接查询 `rsduck_internal.*` 默认被拒绝。
- internal mutation planner 可以创建和删除 `rsduck_internal` physical table。

### 15.6 账号与权限

- 未认证 PG wire session 不能执行 SQL。
- 禁用用户登录失败。
- `reader` 只能查询被授权 relation，不能写入。
- `writer` 可写入被授权 relation，但不能执行 DDL。
- `ddl` 可在被授权 schema 下创建用户对象。
- `operator` 可执行 snapshot 和 catalog 诊断，但不能自动读取未授权用户 relation。
- `admin` 可管理用户、角色和权限。
- `has_table_privilege` 对有权限和无权限 relation 返回不同结果。
- 普通用户不能查询 `rsduck_catalog.*` 和 `rsduck_internal.*`。
- reserved schema 诊断访问必须记录审计事件。

## 16. 实现顺序约束

实现时不得先绕过 catalog 直接做 DuckDB DDL。正确顺序是：

```text
1. catalog bootstrap
2. OID allocator
3. user / role / privilege bootstrap
4. authentication and authorization guard
5. catalog mutation transaction
6. pg_catalog / information_schema projection
7. reserved schema guard
8. managed range partitioned table
9. startup consistency check
10. client compatibility tests
```

这个顺序是依赖关系，不是产品分期。任一能力如果没有接入 catalog mutation 和恢复校验，就不能算完成。

# DuckDB 直连展示层重构清单

本文规划 MySQL 展示层逐步从 `rsduck_catalog.rs_*` 读取对象元数据，调整为直接读取 DuckDB metadata table functions。`rs_*` catalog 继续负责权限、对象状态、分区、DDL 事务、Snapshot v2 和恢复校验。

## 1. 目标与边界

- [ ] MySQL 客户端看到的表、视图、函数、列、索引优先反映 DuckDB 的真实物理状态。
- [ ] `information_schema`、`SHOW ...` 只作为 MySQL-compatible 投影，不成为新的元数据事实来源。
- [ ] `rsduck_catalog` 保留对象权限、`unavailable` 状态、分区逻辑和快照治理数据。
- [ ] 不新增 `rs_function`、`rs_view` 等重复保存 DuckDB 元数据的 catalog 表。
- [ ] 不允许展示层直接暴露 `rsduck_catalog`、`rsduck_internal` 或无权限对象。
- [ ] 不在本阶段删除现有 `rs_*` 表；删除必须单独完成迁移、恢复和回归验证。

## 2. 统一展示层接口

- [x] 在 `src/mysql_compat.rs` 中统一实现 DuckDB metadata projection builder。
- [x] 将 projection 分为两层：
  - DuckDB facts：`duckdb_tables()`、`duckdb_columns()`、`duckdb_indexes()`、`duckdb_views()`、`duckdb_functions()`。
  - rsduck policy overlay：用户权限、`rs_relation.status`、`rs_relation_ext.visibility`、分区入口可见性。
- [x] 禁止 MySQL metadata SQL 直接查询 `rsduck_catalog` 表作为对象列表主来源。
- [x] 为 DuckDB metadata table function 建立受控授权规则，避免它被当作普通业务 relation 授权失败。
- [ ] 保留未支持 MySQL metadata relation 的明确错误，不回退到 DuckDB `information_schema`。

## 3. 阶段一：视图与函数

### 3.1 视图

- [x] `duckdb_views()` 映射到 `information_schema.views`。
- [x] 投影 `TABLE_SCHEMA`、`TABLE_NAME`、`VIEW_DEFINITION`、`CHECK_OPTION`、`IS_UPDATABLE`、`DEFINER`、`SECURITY_TYPE` 等 MySQL 字段。
- [x] `SHOW FULL TABLES` 和 `SHOW TABLE STATUS` 的视图类型来自 DuckDB 真实视图对象。
- [x] 过滤系统 schema、内部物理分区和 catalog-only 对象。
- [x] 对 catalog 标记为 `unavailable` 的视图保持不可见或按既定状态策略展示。

### 3.2 函数与 macro

- [x] `duckdb_functions()` 映射到 `information_schema.routines`。
- [x] 仅将用户 schema 的 DuckDB macro 映射为 MySQL `FUNCTION`；DuckDB 内置函数不写入 rsduck catalog。
- [x] `duckdb_functions().parameters` 映射到 `information_schema.parameters`，确认 DuckDB 当前版本的参数字段和 overload 表达方式。
- [x] `SHOW FUNCTION STATUS` 返回用户 macro/function 投影。
- [x] `SHOW PROCEDURE STATUS` 返回标准空结果集；DuckDB 没有 MySQL stored procedure 对象模型。
- [x] Navicat 函数节点的 `SHOW PROCEDURE STATUS`、`SHOW FUNCTION STATUS`、`information_schema.routines`、`information_schema.parameters` 查询全部无错误。

## 4. 阶段二：表、列与索引

- [x] `duckdb_tables()` 映射到 `information_schema.tables` 和 `SHOW TABLES`。
- [x] `duckdb_columns()` 映射到 `information_schema.columns`、`SHOW COLUMNS`、`DESCRIBE`。
- [x] `duckdb_indexes()` 映射到 `information_schema.statistics` 和 `SHOW INDEX`。
- [x] 保留 rsduck policy overlay：
  - 授权对象只能出现在结果中。
  - `unavailable` 对象按统一规则隐藏或标记。
  - 分区父表展示为业务对象，`rsduck_internal` 子分区不展示。
  - catalog 与 DuckDB 不一致时返回明确诊断，不选择任意一侧静默兜底。
- [x] MySQL metadata projection 不再依赖 `rs_type`、`rs_column`、`rs_index` 作为展示字段的主来源。

## 5. 阶段三：Snapshot v2 与恢复

- [x] `manifest.json` 增加 `views[]`，记录 schema、name、DDL、checksum 或等价摘要。
- [x] `manifest.json` 增加 `macros[]`，记录 schema、name、类型、参数、定义 DDL。
- [x] snapshot save 从 DuckDB metadata 读取用户视图与 macro 定义。
- [x] restore 顺序固定为：catalog -> schema -> 业务数据 -> index -> view -> macro/function -> 分区入口校验。
- [x] 用户视图或 macro DDL 损坏时按对象级 `unavailable` 或明确恢复失败策略处理。
- [x] 增加视图、scalar macro、table macro 的 snapshot save/restore 测试。

## 6. 阶段四：catalog 精简评估

以下表不在前述展示层改造中直接删除，先完成替代设计和迁移验证。

- [x] 本轮完成依赖审计：没有任何 `rs_*` catalog 表满足“可直接删除”条件；本阶段不删除表，避免破坏分区重建、权限、DDL 事务和 Snapshot restore。

- [ ] `rs_type`
  - 候选方案：将 `rs_column` 的类型改为稳定 `sql_type` / `physical_type` 文本，分区 DDL 与校验直接使用 DuckDB 类型。
  - 前置条件：分区重建、字段校验、Snapshot v2 restore 不再依赖 type ID lookup。
- [ ] `rs_column_default`
  - 候选方案：合并到 `rs_column.default_expr`。
  - 前置条件：ALTER TABLE、分区创建和恢复均能保留 default expression。
- [ ] `rs_comment`
  - 候选方案：优先读取 DuckDB comment metadata；保留无法由 DuckDB 表达的应用级 comment。
  - 前置条件：MySQL table/column comment、Snapshot v2、COMMENT ON 回归测试通过。
- [ ] `rs_index`
  - 暂不删除：普通索引和分区索引恢复需要逻辑索引定义。
- [ ] `rs_dependency`
  - 暂不删除：DROP CASCADE、视图依赖、分区入口依赖校验需要稳定依赖图。
- [ ] `rs_schema`、`rs_relation`、`rs_column`、`rs_relation_ext`、`rs_partition`
  - 暂不删除：权限、对象状态、分区治理、Snapshot v2 relation 清单依赖这些表。
- [ ] `rs_catalog_version`、`rs_oid_alloc`、`rs_catalog_journal`、`rs_user`、`rs_role`、`rs_user_role`、`rs_privilege`
  - 不属于展示层替代范围，继续保留。

## 7. 每阶段完成标准

- [x] MySQL protocol、prepared statement、Web SQL API 的 metadata 查询走同一投影路径。
- [x] Navicat 的表、视图、函数节点点击与新建查询不报错。
- [x] 无权限用户不能通过 metadata 枚举获得对象名称。
- [x] Snapshot v2 restore 后 DuckDB 物理对象、展示层和 rsduck policy overlay 一致。
- [x] `cargo fmt`、`cargo check`、`cargo test` 和 UTF-8 严格扫描通过。

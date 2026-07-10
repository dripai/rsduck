# MySQL-only Catalog 重构操作清单

本文记录 rsduck 从 PG-compatible catalog 模型重构为 MySQL-only 产品形态的实施清单。目标是让 DuckDB 运行库只保留 `rsduck_catalog.rs_*` 内部事实表，MySQL wire 作为唯一外部数据库协议入口，`information_schema` / `SHOW ...` 从新的 `rs_*` catalog 投影生成。

## 1. 产品边界

- [x] 移除 PostgreSQL wire listener 的启动路径和配置项。
- [x] 移除主执行路径对 `pg_compat` 的依赖。
- [x] `pg_catalog.*` 不再作为外部查询契约。
- [x] `information_schema.*` 保留，但语义改为 MySQL-compatible 投影。
- [x] Web API / 控制台列元数据不再返回 `pg_type_oid`，改为返回中性 `sql_type` 和必要的 MySQL 展示类型。
- [x] 不保留 PG/MySQL 双模式开关；当前产品只走 MySQL-only 路径。
- [x] 不做旧 catalog 或旧 snapshot 的隐式兼容读取；需要迁移时使用显式离线迁移命令。

## 2. 新 catalog schema

在 `rsduck_catalog` schema 中建立新的内部事实表，表名统一使用 `rs_` 前缀。

- [x] `rs_catalog_version`
  - 保留现有职责，增加 `snapshot_format_version` 或等价字段。
  - 记录 `catalog_epoch`、`catalog_checksum`、`schema_version`、`status`。
- [x] `rs_oid_alloc`
  - 保留全局对象 ID 分配。
- [x] `rs_catalog_journal`
  - 保留 catalog mutation 事务日志。
- [x] `rs_schema`
  - 替代 `pg_namespace`。
  - 字段建议：`schema_id`、`schema_name`、`owner_user_id`、`status`、`created_at`、`updated_at`。
- [x] `rs_relation`
  - 替代 `pg_class`。
  - 字段建议：`relation_id`、`schema_id`、`relation_name`、`relation_kind`、`owner_user_id`、`storage_schema`、`storage_name`、`row_estimate`、`status`、`error_message`、`created_at`、`updated_at`。
  - `relation_kind` 初始范围：`table`、`view`、`index`、`partitioned_table`、`physical_partition`。
- [x] `rs_column`
  - 替代 `pg_attribute`。
  - 字段建议：`relation_id`、`column_id`、`column_name`、`ordinal_position`、`sql_type`、`physical_type`、`nullable`、`default_expr`、`generated_expr`、`is_dropped`、`created_at`、`updated_at`。
- [x] `rs_constraint`
  - 替代 `pg_constraint`。
  - 字段建议：`constraint_id`、`relation_id`、`constraint_name`、`constraint_type`、`column_ids`、`ref_relation_id`、`ref_column_ids`、`check_expr`、`is_validated`。
- [x] `rs_index`
  - 替代 `pg_index` 和 index relation 的 PG 元数据。
  - 字段建议：`index_id`、`relation_id`、`index_name`、`column_ids`、`is_unique`、`is_primary`、`is_valid`、`predicate_expr`、`storage_name`。
- [x] `rs_dependency`
  - 替代 `pg_depend`。
  - 字段建议：`object_type`、`object_id`、`ref_object_type`、`ref_object_id`、`dependency_type`。
- [x] `rs_comment`
  - 替代 `pg_description`。
  - 字段建议：`object_type`、`object_id`、`sub_object_id`、`comment_text`。
- [x] 保留并按新 relation ID 语义调整：
  - `rs_relation_ext`
  - `rs_partition`
  - `rs_user`
  - `rs_role`
  - `rs_user_role`
  - `rs_privilege`

## 3. Catalog bootstrap

- [x] 改造 `src/catalog/storage.rs`，只创建 `rs_*` 表。
- [x] 改造 `src/catalog/bootstrap.rs`，初始化内置 schema：
  - `information_schema`
  - `rsduck_catalog`
  - `rsduck_internal`
  - `main`
- [x] 初始化内置用户、角色和系统权限。
- [x] 删除 PG 内置 type OID 初始化，改为内部 `sql_type` 枚举或稳定文本类型码。
- [x] 改造 checksum 计算，覆盖新的 `rs_*` 表集合。
- [x] 增加 bootstrap 后 UTF-8 和 checksum 测试。

## 4. DDL mutation 迁移

所有 catalog-aware DDL 必须先写入新的 `rs_*` catalog，再执行 DuckDB physical DDL，仍通过 `run_catalog_tx`、journal、epoch、checksum 统一提交。

- [x] `CREATE SCHEMA` 写入 `rs_schema`。
- [x] `CREATE TABLE` 写入 `rs_relation`、`rs_column`、`rs_constraint`、`rs_relation_ext`。
- [x] `CREATE VIEW` 写入 `rs_relation`、`rs_column`、`rs_dependency`、`rs_relation_ext`。
- [x] `CREATE INDEX` 写入 `rs_index`，并更新 relation index 状态。
- [x] `ALTER TABLE ADD COLUMN` 写入 `rs_column` 并同步 DuckDB DDL。
- [x] `ALTER TABLE DROP COLUMN` 标记 `rs_column.is_dropped = true`，并同步 DuckDB DDL。
- [x] `DROP TABLE/VIEW/INDEX/SCHEMA` 清理或标记对应 `rs_*` 元数据。
- [x] `COMMENT ON ...` 写入 `rs_comment`。
- [x] `GRANT/REVOKE` 改为基于 `rs_schema` / `rs_relation` 对象 ID 授权。
- [x] 分区表创建、维护、过期、修复改为读写 `rs_relation`、`rs_column`、`rs_dependency`、`rs_relation_ext`、`rs_partition`。
- [x] 删除所有对 `rsduck_catalog.pg_*` 的写入。

## 5. MySQL metadata 投影

MySQL 客户端的元数据查询不直接访问 catalog 表，而是通过受控投影 SQL 或内置结果集返回。

- [x] `SHOW TABLES` 从 `rs_schema` / `rs_relation` 投影。
- [x] `SHOW FULL TABLES` 从 `rs_schema` / `rs_relation` 投影。
- [x] `SHOW TABLE STATUS` 从 `rs_relation` 和统计字段投影。
- [x] `SHOW COLUMNS` / `DESCRIBE` 从 `rs_column` 投影。
- [x] `SHOW INDEX` 从 `rs_index` 投影。
- [x] `information_schema.schemata` 从 `rs_schema` 投影。
- [x] `information_schema.tables` 从 `rs_relation` 投影。
- [x] `information_schema.columns` 从 `rs_column` 投影。
- [x] `information_schema.statistics` 从 `rs_index` 投影。
- [x] `information_schema.table_constraints` 从 `rs_constraint` 投影。
- [x] `information_schema.key_column_usage` 从 `rs_constraint` 投影。
- [x] `information_schema.engines` 继续返回固定兼容结果。
- [x] 未支持的 `information_schema` relation 返回明确错误或定义好的空结果。
- [x] 删除通过 `pg_compat::rewrite_sql` 生成 MySQL metadata 的路径。

## 6. SQL 执行路径

- [x] `execute_typed_sql_blocking` 不再调用 `pg_compat::rewrite_sql`。
- [x] 新增 MySQL metadata router，专门识别 `information_schema.*` 和 `SHOW ...`。
- [x] reserved schema guard 保留，但错误信息改成 MySQL-only 语义。
- [x] 权限校验改为基于 `rs_schema` / `rs_relation`。
- [x] `describe_sql_blocking` 对 MySQL metadata 查询返回真实 `SqlColumn` 类型。
- [x] 不增加 DuckDB introspection fallback；catalog 缺失时返回明确错误。

## 7. Snapshot v2 格式

目标格式：

```text
snapshot/rsduck-YYYYMMDD-HHMMSS/
  manifest.json
  catalog.duckdb
  data/
    main.table_name.parquet
    rsduck_internal.physical_partition.parquet
```

- [x] `manifest.json` 记录：
  - `snapshot_format_version`
  - `created_at`
  - `catalog_epoch`
  - `catalog_checksum`
  - `rsduck_version`
  - `tables[]`
  - `partitions[]`
  - 每个数据文件的 schema、relation、row_count、checksum 或统计摘要。
- [x] `catalog.duckdb` 只包含 `rsduck_catalog.rs_*` 表。
- [x] `data/*.parquet` 只包含业务表和必要的 `rsduck_internal` 物理分区表。
- [x] snapshot save 通过写队列串行执行，避免 catalog 和数据文件跨 epoch。
- [x] 导出前读取当前 `catalog_epoch`。
- [x] 导出 `catalog.duckdb`。
- [x] 按 active catalog relation 清单导出数据表。
- [x] 导出后重新读取 `catalog_epoch`，若 epoch 改变则本次 snapshot 失败并清理目录。
- [x] 不再使用整库 `EXPORT DATABASE` 作为主 snapshot 格式。

## 8. Restore v2 流程

- [x] 读取并校验 `manifest.json`。
- [x] 打开 `catalog.duckdb`，校验 `rs_catalog_version`。
- [x] 校验 `catalog_checksum`。
- [x] 在新的 in-memory DuckDB 中创建 `rsduck_catalog` 和 `rsduck_internal`。
- [x] 从 `catalog.duckdb` 复制 `rs_*` catalog 表。
- [x] 按 manifest 和 catalog 导入业务 Parquet 文件。
- [x] 校验 active relation 对应的 physical object 是否存在。
- [x] 校验字段顺序、字段名和 `physical_type`。
- [x] 重建或校验分区表 generated view。
- [x] 单个业务对象损坏时按既定策略标记 `unavailable`；全局 catalog 损坏时启动失败。
- [x] 不从 DuckDB 物理表反推 catalog。

## 9. 旧数据迁移

- [x] 新增显式离线迁移命令，例如 `rsduck migrate-snapshot --from <old> --to <new>`。
- [x] 迁移命令读取旧 snapshot，构造新的 `rs_*` catalog，再写出 Snapshot v2。
- [x] 正常启动路径不自动迁移旧 snapshot。
- [x] 旧格式缺失或不受支持时返回明确错误。

## 10. 测试清单

- [x] `rs_*` catalog bootstrap 测试。
- [x] catalog checksum 测试。
- [x] 用户、角色、权限认证测试。
- [x] DDL mutation 测试：
  - schema
  - table
  - view
  - index
  - constraint
  - comment
  - drop
  - alter table
- [x] MySQL wire text query 测试。
- [x] MySQL prepared statement 测试。
- [x] MySQL `SHOW ...` 测试。
- [x] MySQL `information_schema.*` 测试。
- [x] 分区表创建、维护、过期、修复测试。
- [x] Snapshot v2 save/restore 测试。
- [x] 损坏 catalog 启动失败测试。
- [x] 损坏业务表标记 unavailable 测试。
- [x] UTF-8 严格扫描。

## 11. 建议实施顺序

1. 新增 `rs_*` catalog schema 和 bootstrap 测试。
2. 迁移 catalog lookup、权限和 DDL mutation。
3. 切换 MySQL metadata router 和 `information_schema` 投影。
4. 移除 PG listener、`pg_compat` 主路径和 PG-only 文档契约。
5. 实现 Snapshot v2 save。
6. 实现 Snapshot v2 restore。
7. 增加显式旧 snapshot 迁移命令。
8. 清理依赖、测试和文档。

## 12. 完成标准

- [x] 新建库启动后没有 `rsduck_catalog.pg_*` 表。
- [x] MySQL 客户端可以完成登录、查询、prepared statement、`SHOW TABLES`、`SHOW COLUMNS`、`information_schema` 基础探测。
- [x] Web/API 不暴露 `pg_type_oid`。
- [x] 快照目录包含 `manifest.json`、`catalog.duckdb`、`data/*.parquet`。
- [x] 从 Snapshot v2 恢复后 catalog checksum 和业务数据校验通过。
- [x] 旧 snapshot 不被正常启动路径隐式兼容。
- [x] `cargo fmt`、`cargo check`、`cargo test`、UTF-8 扫描通过。

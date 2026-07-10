# RSDuck实战样例

本文按两类场景组织：

- 单条 SQL：在 Navicat、MySQL 客户端、RSDuck Web SQL 页面直接执行。
- 程序化任务：由后端接口、定时任务或数据同步流程执行。

约定：示例股票代码格式为 `688981.SH`、`603986.SH`、`300661.SZ`；批次号格式为 `batch_20260710_001`。

## 1. 操作边界

单条 SQL：

- 查看对象：`SHOW TABLES`、`DESCRIBE`
- 创建对象：`CREATE TABLE`、`CREATE VIEW`
- 小范围修改：`ALTER TABLE`、`COMMENT ON`、带明确 `WHERE` 的 `UPDATE` / `DELETE`

程序化任务：

- 批量同步板块和成分股
- 导入 Parquet 文件
- 管理分区表
- 聚合行情指标
- 执行数据质量检查
- 保存和恢复 snapshot

Web SQL 页面应对 `ALTER`、`UPDATE`、`DELETE`、`DROP` 标记高风险；没有 `WHERE` 的 `UPDATE` / `DELETE` 应拒绝或要求管理员确认。

## 2. 普通表

板块主表只保存板块自身信息。

```sql
CREATE TABLE sector_list (
  sector_code VARCHAR,
  sector_name VARCHAR,
  category VARCHAR,
  constituent_count INTEGER,
  source VARCHAR,
  ingest_batch_id VARCHAR,
  ingest_at TIMESTAMP
);
```

字段说明：

- `sector_code`：板块代码，全局唯一。
- `sector_name`：板块名称。
- `category`：板块分类，例如 `concept`、`sw_industry`、`region`、`index`、`fund_etf`。
- `constituent_count`：成分股数量，便于列表页展示和质量检查。
- `source`：数据来源，例如 `xtquant`。
- `ingest_batch_id`：同步批次号。
- `ingest_at`：写入时间。

插入样例：

```sql
INSERT INTO sector_list
VALUES
  ('GN_SEMI', '半导体', 'concept', 3, 'xtquant', 'batch_20260710_001', now()),
  ('SW_ELEC', '电子', 'sw_industry', 2, 'xtquant', 'batch_20260710_001', now());
```

检查写入结果：

```sql
SELECT *
FROM sector_list
ORDER BY sector_code;
```

## 3. 成分股表

板块和股票是多对多关系。一行保存一个板块成分，便于反查、关联行情和聚合统计。

```sql
CREATE TABLE sector_constituents (
  sector_code VARCHAR,
  stock_code VARCHAR,
  ingest_batch_id VARCHAR,
  ingest_at TIMESTAMP
);
```

插入样例：

```sql
INSERT INTO sector_constituents
VALUES
  ('GN_SEMI', '688981.SH', 'batch_20260710_001', now()),
  ('GN_SEMI', '603986.SH', 'batch_20260710_001', now()),
  ('GN_SEMI', '300661.SZ', 'batch_20260710_001', now());
```

查询某个板块的成分股：

```sql
SELECT stock_code
FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

查询某只股票属于哪些板块：

```sql
SELECT
  s.sector_code,
  s.sector_name,
  s.category
FROM sector_constituents c
JOIN sector_list s
  ON s.sector_code = c.sector_code
WHERE c.stock_code = '688981.SH'
ORDER BY s.category, s.sector_code;
```

检查重复成分：

```sql
SELECT
  sector_code,
  stock_code,
  count(*) AS duplicate_count
FROM sector_constituents
GROUP BY sector_code, stock_code
HAVING count(*) > 1;
```

## 4. LIST 字段

DuckDB 支持 `LIST` 类型，常用写法是 `VARCHAR[]`。用于展示型快照，不替代明细关系表。

```sql
CREATE TABLE sector_snapshot (
  sector_code VARCHAR,
  sector_name VARCHAR,
  stock_codes VARCHAR[],
  ingest_at TIMESTAMP
);
```

插入样例：

```sql
INSERT INTO sector_snapshot
VALUES
  ('GN_SEMI', '半导体', ['688981.SH', '603986.SH', '300661.SZ'], now());
```

判断列表中是否包含某只股票：

```sql
SELECT *
FROM sector_snapshot
WHERE list_contains(stock_codes, '688981.SH');
```

把列表拆成明细行：

```sql
SELECT
  sector_code,
  sector_name,
  unnest(stock_codes) AS stock_code
FROM sector_snapshot;
```

使用规则：

- 页面展示“某板块包含哪些股票”：可以用 `VARCHAR[]`。
- 查询“某股票属于哪些板块”：用 `sector_constituents`。
- 关联 K 线、统计涨跌幅、做板块聚合：用 `sector_constituents`。

## 5. 分区表

RSDuck 的分区表使用范围分区语法，不直接暴露 DuckDB 的 Hive 目录分区数据集。业务侧只操作逻辑表，物理分区由 RSDuck 在 `rsduck_internal` 下创建和维护。

创建按日分区、保留 30 个分区的分钟线表：

```sql
CREATE TABLE kline_1m (
  stock_code VARCHAR NOT NULL,
  trade_time TIMESTAMP NOT NULL,
  open DOUBLE,
  high DOUBLE,
  low DOUBLE,
  close DOUBLE,
  volume BIGINT,
  amount DOUBLE,
  ingest_batch_id VARCHAR,
  ingest_at TIMESTAMP,
  PRIMARY KEY (stock_code, trade_time)
)
PARTITION BY RANGE (trade_time)
WITH (
  partition_unit = 'day',
  retention = '30'
);
```

写入时只写逻辑表：

```sql
INSERT INTO kline_1m (
  stock_code,
  trade_time,
  open,
  high,
  low,
  close,
  volume,
  amount,
  ingest_batch_id,
  ingest_at
)
VALUES
  ('688981.SH', TIMESTAMP '2026-07-10 09:31:00', 50.1, 50.4, 50.0, 50.2, 120000, 6024000, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:32:00', 50.2, 50.5, 50.1, 50.3, 98000, 4929400, 'batch_20260710_001', now());
```

查询仍然查询逻辑表：

```sql
SELECT
  stock_code,
  trade_time,
  close,
  volume
FROM kline_1m
WHERE stock_code = '688981.SH'
  AND trade_time >= TIMESTAMP '2026-07-10 09:30:00'
  AND trade_time < TIMESTAMP '2026-07-10 15:00:00'
ORDER BY trade_time;
```

查看分区状态：

```sql
SHOW PARTITIONS FROM kline_1m;
```

维护命令：

```sql
CALL rsduck_run_partition_maintenance();
```

如果某个物理分区异常，可以先标记再修复：

```sql
CALL rsduck_mark_partition_unavailable(
  'kline_1m',
  '20260710',
  'manual check'
);

CALL rsduck_repair_partition('kline_1m', '20260710');
```

使用规则：

- 分区键必须是 `DATE` 或 `TIMESTAMP`，且必须 `NOT NULL`。
- `partition_unit` 支持 `hour`、`day`、`month`、`year`。
- 外部不要直接操作 `rsduck_internal` 下的物理分区。
- 需要导入外部 Parquet 文件时，使用 Web 的 Parquet 导入入口，不把它和分区表混在一个示例里。

## 6. 视图

视图用于固化常用查询。

```sql
CREATE VIEW v_sector_constituents AS
SELECT
  s.sector_code,
  s.sector_name,
  s.category,
  c.stock_code,
  c.ingest_batch_id,
  c.ingest_at
FROM sector_list s
JOIN sector_constituents c
  ON s.sector_code = c.sector_code;
```

查询视图：

```sql
SELECT *
FROM v_sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

替换视图：

```sql
CREATE OR REPLACE VIEW v_sector_constituents AS
SELECT
  s.sector_code,
  s.sector_name,
  s.category,
  c.stock_code
FROM sector_list s
JOIN sector_constituents c
  ON s.sector_code = c.sector_code;
```

## 7. 修改表结构

增加字段：

```sql
ALTER TABLE sector_list ADD COLUMN description VARCHAR;
```

删除字段：

```sql
ALTER TABLE sector_list DROP COLUMN description;
```

修改字段名：

```sql
ALTER TABLE sector_list RENAME COLUMN sector_name TO name;
```

修改字段类型：

```sql
ALTER TABLE sector_list ALTER COLUMN constituent_count SET DATA TYPE BIGINT;
```

修改后检查：

```sql
DESCRIBE sector_list;
```

注意点：

- 删除字段前先确认下游视图、查询、导出任务是否依赖该字段。
- 修改字段类型前先检查现有数据能否转换。
- 结构变更应写入操作日志。

## 8. 修改元数据

表和字段说明用 `COMMENT ON`。

```sql
COMMENT ON TABLE sector_list IS '股票板块主表';
```

```sql
COMMENT ON COLUMN sector_list.category IS '板块分类：concept、sw_industry、region、index 等';
```

管理信息单独建表保存。

```sql
CREATE TABLE data_catalog (
  object_name VARCHAR,
  object_type VARCHAR,
  display_name VARCHAR,
  description VARCHAR,
  owner VARCHAR,
  data_source VARCHAR,
  refresh_mode VARCHAR,
  refresh_cron VARCHAR,
  risk_level VARCHAR,
  updated_at TIMESTAMP
);
```

写入样例：

```sql
INSERT INTO data_catalog
VALUES (
  'sector_constituents',
  'table',
  '板块成分股表',
  '一行代表一个板块成分股',
  'research',
  'xtquant',
  'scheduled',
  '0 30 18 * * 1-5',
  'medium',
  now()
);
```

查询数据目录：

```sql
SELECT *
FROM data_catalog
WHERE object_name = 'sector_constituents';
```

## 9. 修改记录

修改板块名称：

```sql
UPDATE sector_list
SET sector_name = '半导体'
WHERE sector_code = 'GN_SEMI';
```

修改板块分类：

```sql
UPDATE sector_list
SET category = 'concept'
WHERE sector_code = 'GN_SEMI';
```

删除某个成分股：

```sql
DELETE FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
  AND stock_code = '300661.SZ';
```

修改后校验：

```sql
SELECT *
FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

执行规则：

- `UPDATE` / `DELETE` 必须带明确 `WHERE`。
- 批量修改优先走程序化任务。
- 高风险修改记录执行人、SQL 摘要、影响行数。

## 10. 查看对象和结构

查看表：

```sql
SHOW TABLES;
```

查看字段：

```sql
DESCRIBE sector_list;
```

查看样例数据：

```sql
SELECT *
FROM sector_list
LIMIT 20;
```

查看视图：

```sql
SELECT *
FROM v_sector_constituents
LIMIT 20;
```

放在 Web SQL 样例库首页，风险等级标记为低。

## 11. SQL 样例库

样例字段：

- `title`：样例名称。
- `category`：查询、建表、结构变更、数据修改、数据工程。
- `risk_level`：low、medium、high。
- `entry`：Navicat、Web SQL、后端任务。
- `sql_template`：SQL 模板。
- `params`：可替换参数。

样例表：

```sql
CREATE TABLE sql_example_catalog (
  title VARCHAR,
  category VARCHAR,
  risk_level VARCHAR,
  entry VARCHAR,
  sql_template VARCHAR,
  params VARCHAR[],
  updated_at TIMESTAMP
);
```

示例数据：

```sql
INSERT INTO sql_example_catalog
VALUES (
  '查询板块成分股',
  '查询',
  'low',
  'Web SQL',
  'SELECT stock_code FROM sector_constituents WHERE sector_code = $sector_code ORDER BY stock_code',
  ['sector_code'],
  now()
);
```

页面规则：

- 低风险样例允许直接运行。
- 中风险样例显示确认弹窗。
- 高风险样例要求管理员权限和二次确认。

## 12. 程序化场景：板块成分全量同步

流程：

1. 生成 `ingest_batch_id`。
2. 拉取板块列表和成分股列表。
3. 写入 staging 表。
4. 检查空板块、重复成分、非法股票代码。
5. 事务内替换正式表。
6. 写入同步日志。

同步日志表：

```sql
CREATE TABLE sector_sync_log (
  ingest_batch_id VARCHAR,
  source VARCHAR,
  started_at TIMESTAMP,
  finished_at TIMESTAMP,
  sector_count INTEGER,
  constituent_count INTEGER,
  status VARCHAR,
  message VARCHAR
);
```

核心要求：

- 新批次校验失败时不覆盖旧数据。
- 每次同步必须能按批次追踪。
- 失败信息写入 `message`。

## 13. 程序化场景：单个板块增量刷新

输入参数：

- `sector_code`
- `source`
- `ingest_batch_id`

事务 SQL 形态：

```sql
BEGIN TRANSACTION;

DELETE FROM sector_constituents
WHERE sector_code = 'GN_SEMI';

INSERT INTO sector_constituents
VALUES
  ('GN_SEMI', '688981.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '603986.SH', 'batch_20260710_002', now());

UPDATE sector_list
SET constituent_count = 2,
    ingest_batch_id = 'batch_20260710_002',
    ingest_at = now()
WHERE sector_code = 'GN_SEMI';

COMMIT;
```

失败处理：

- 拉取失败：不进入事务。
- 校验失败：返回错误，不改正式表。
- 写入失败：回滚事务。

## 14. 程序化场景：板块行情聚合

结果表：

```sql
CREATE TABLE sector_daily_stats (
  sector_code VARCHAR,
  trade_date DATE,
  stock_count INTEGER,
  up_count INTEGER,
  down_count INTEGER,
  avg_pct_chg DOUBLE,
  total_amount DOUBLE,
  ingest_batch_id VARCHAR,
  ingest_at TIMESTAMP
);
```

按交易日生成统计：

```sql
INSERT INTO sector_daily_stats
SELECT
  c.sector_code,
  k.trade_date,
  count(*) AS stock_count,
  sum(CASE WHEN k.pct_chg > 0 THEN 1 ELSE 0 END) AS up_count,
  sum(CASE WHEN k.pct_chg < 0 THEN 1 ELSE 0 END) AS down_count,
  avg(k.pct_chg) AS avg_pct_chg,
  sum(k.amount) AS total_amount,
  'batch_20260710_003' AS ingest_batch_id,
  now() AS ingest_at
FROM sector_constituents c
JOIN kline_1d k
  ON k.symbol = c.stock_code
WHERE k.trade_date = DATE '2026-07-10'
GROUP BY c.sector_code, k.trade_date;
```

查询某天板块排行：

```sql
SELECT *
FROM sector_daily_stats
WHERE trade_date = DATE '2026-07-10'
ORDER BY avg_pct_chg DESC
LIMIT 20;
```

## 15. 程序化场景：Parquet 导入

单文件导入：

```sql
CREATE TABLE imported_daily_quote AS
SELECT *
FROM read_parquet('snapshot/import/daily_quote.parquet');
```

目录读取：

```sql
SELECT *
FROM read_parquet('snapshot/import/daily_quote/*.parquet');
```

导入页面需要做：

- 限制文件路径在允许目录内。
- 导入前预览字段和行数。
- 校验目标表名。
- 批量导入失败时整体回滚。
- 导入成功后写入 `data_catalog`。

## 16. 程序化场景：数据质量检查

检查没有成分股的板块：

```sql
SELECT
  s.sector_code,
  s.sector_name
FROM sector_list s
LEFT JOIN sector_constituents c
  ON s.sector_code = c.sector_code
GROUP BY s.sector_code, s.sector_name
HAVING count(c.stock_code) = 0;
```

检查成分数量不一致：

```sql
SELECT
  s.sector_code,
  s.constituent_count AS recorded_count,
  count(c.stock_code) AS actual_count
FROM sector_list s
LEFT JOIN sector_constituents c
  ON s.sector_code = c.sector_code
GROUP BY s.sector_code, s.constituent_count
HAVING s.constituent_count <> count(c.stock_code);
```

检查非法股票代码：

```sql
SELECT stock_code
FROM sector_constituents
WHERE NOT regexp_matches(stock_code, '^[0-9]{6}\\.(SH|SZ|BJ)$');
```

结果分级：

- error：阻止发布。
- warning：允许发布，但页面展示。
- info：只写日志。

## 17. 程序化场景：快照与恢复

快照入口：

- 系统定时快照。
- Web 手工快照。
- 服务关闭前快照。

页面展示字段：

- 快照时间。
- 触发方式。
- 对象数量。
- 数据文件数量。
- manifest 校验状态。
- 是否可恢复。

权限要求：

- 普通用户不能保存和恢复 snapshot。
- 管理员操作需要审计日志。
- 恢复前展示目标快照的 manifest 信息。

## 18. 程序化场景：权限和审计

创建分析角色：

```sql
CREATE ROLE analyst;
```

授权读取板块表：

```sql
GRANT SELECT ON TABLE sector_list TO ROLE analyst;
GRANT SELECT ON TABLE sector_constituents TO ROLE analyst;
```

审计日志字段：

```sql
CREATE TABLE sql_audit_log (
  username VARCHAR,
  action VARCHAR,
  risk_level VARCHAR,
  sql_digest VARCHAR,
  affected_rows BIGINT,
  status VARCHAR,
  message VARCHAR,
  created_at TIMESTAMP
);
```

记录范围：

- DDL：`CREATE`、`ALTER`、`DROP`
- DML：`INSERT`、`UPDATE`、`DELETE`
- 系统操作：导入、快照、恢复、权限变更

## 19. 页面落地

SQL 样例库：

- 按风险等级和操作类型筛选。
- 支持参数填写后生成 SQL。
- 低风险可直接运行，高风险必须确认。

数据对象管理：

- 展示表、视图、字段、注释、数据目录信息。
- 展示行数概览和最近更新时间。
- 修改元数据走受控表单，不直接编辑系统表。

数据任务：

- 板块同步。
- Parquet 导入。
- 分区表维护。
- 数据质量检查。
- snapshot 保存和恢复。

每个任务至少展示：状态、开始时间、结束时间、批次号、影响行数、失败原因。

## 20. 参考

- DuckDB `CREATE TABLE`: https://duckdb.org/docs/current/sql/statements/create_table
- DuckDB `ALTER TABLE`: https://duckdb.org/docs/current/sql/statements/alter_table
- DuckDB `CREATE VIEW`: https://duckdb.org/docs/current/sql/statements/create_view
- DuckDB `COMMENT ON`: https://duckdb.org/docs/current/sql/statements/comment_on
- DuckDB `INSERT`: https://duckdb.org/docs/current/sql/statements/insert
- DuckDB `UPDATE`: https://duckdb.org/docs/current/sql/statements/update
- DuckDB `DELETE`: https://duckdb.org/docs/current/sql/statements/delete

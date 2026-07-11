# RSDuck Practical Examples

Language: English | [中文](rsduck-practical-examples.md)

This document is organized into three content groups:

- DDL: create, change, and document tables, views, partitioned tables, and other objects.
- DML and queries: write, update, and delete records, then inspect objects and data.
- Programmatic tasks: executed by backend APIs, scheduled jobs, or data synchronization workflows.

Conventions: sample stock codes use `688981.SH`, `603986.SH`, and `300661.SZ`; batch ids use a format such as `batch_20260710_001`.

## 1. Usage Boundary

Single SQL statements:

- Inspect objects: `SHOW TABLES`, `DESCRIBE`
- Create objects: `CREATE TABLE`, `CREATE VIEW`
- Small-scope changes: `ALTER TABLE`, `COMMENT ON`, and `UPDATE` / `DELETE` with an explicit `WHERE`

Programmatic tasks:

- Batch synchronize sectors and constituents.
- Import Parquet files.
- Manage partitioned tables.
- Aggregate market indicators.
- Run data quality checks.
- Save and restore snapshots.

The Web SQL page should mark `ALTER`, `UPDATE`, `DELETE`, and `DROP` as high-risk operations. `UPDATE` / `DELETE` without `WHERE` should be rejected or require administrator confirmation.

## 2. DDL: Object Definition and Structure Management

### 2.1 Ordinary Table

The sector master table stores only sector-level information.

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

Field notes:

- `sector_code`: globally unique sector code.
- `sector_name`: sector name.
- `category`: sector category, for example `concept`, `sw_industry`, `region`, `index`, `fund_etf`.
- `constituent_count`: number of constituents, useful for list display and quality checks.
- `source`: data source, for example `xtquant`.
- `ingest_batch_id`: ingestion batch id.
- `ingest_at`: write time.

Insert sample data:

```sql
INSERT INTO sector_list
VALUES
  ('GN_SEMI', 'Semiconductor', 'concept', 3, 'xtquant', 'batch_20260710_001', now()),
  ('GN_AI', 'Artificial Intelligence', 'concept', 1, 'xtquant', 'batch_20260710_001', now()),
  ('GN_CLOUD', 'Cloud Computing', 'concept', 1, 'xtquant', 'batch_20260710_001', now()),
  ('SW_ELEC', 'Electronics', 'sw_industry', 1, 'xtquant', 'batch_20260710_001', now()),
  ('SW_COMP', 'Computer Industry', 'sw_industry', 1, 'xtquant', 'batch_20260710_001', now()),
  ('SW_MED', 'Healthcare', 'sw_industry', 1, 'xtquant', 'batch_20260710_001', now()),
  ('REG_BEIJING', 'Beijing Sector', 'region', 1, 'xtquant', 'batch_20260710_001', now()),
  ('INDEX_HS300', 'CSI 300', 'index', 1, 'xtquant', 'batch_20260710_001', now()),
  ('INDEX_ZZ500', 'CSI 500', 'index', 1, 'xtquant', 'batch_20260710_001', now()),
  ('ETF_50', 'SSE 50 ETF', 'fund_etf', 1, 'xtquant', 'batch_20260710_001', now());
```

Check inserted rows:

```sql
SELECT *
FROM sector_list
ORDER BY sector_code;
```

### 2.2 Constituent Table

Sectors and stocks are many-to-many. Store one sector constituent per row. This makes reverse lookup, quote joins, and aggregation straightforward.

```sql
CREATE TABLE sector_constituents (
  sector_code VARCHAR,
  stock_code VARCHAR,
  ingest_batch_id VARCHAR,
  ingest_at TIMESTAMP
);
```

Insert sample data:

```sql
INSERT INTO sector_constituents
VALUES
  ('GN_SEMI', '688981.SH', 'batch_20260710_001', now()),
  ('GN_SEMI', '603986.SH', 'batch_20260710_001', now()),
  ('GN_SEMI', '300661.SZ', 'batch_20260710_001', now()),
  ('GN_AI', '688111.SH', 'batch_20260710_001', now()),
  ('GN_CLOUD', '600570.SH', 'batch_20260710_001', now()),
  ('SW_ELEC', '002049.SZ', 'batch_20260710_001', now()),
  ('SW_COMP', '002415.SZ', 'batch_20260710_001', now()),
  ('SW_MED', '300760.SZ', 'batch_20260710_001', now()),
  ('REG_BEIJING', '688981.SH', 'batch_20260710_001', now()),
  ('INDEX_HS300', '600519.SH', 'batch_20260710_001', now()),
  ('INDEX_ZZ500', '300750.SZ', 'batch_20260710_001', now()),
  ('ETF_50', '510050.SH', 'batch_20260710_001', now());
```

Query constituents of one sector:

```sql
SELECT stock_code
FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

Query which sectors a stock belongs to:

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

Check duplicated constituents:

```sql
SELECT
  sector_code,
  stock_code,
  count(*) AS duplicate_count
FROM sector_constituents
GROUP BY sector_code, stock_code
HAVING count(*) > 1;
```

### 2.3 LIST Columns

DuckDB supports `LIST` columns. The common syntax is `VARCHAR[]`. Use LIST columns for display snapshots, not as a replacement for detail relation tables.

```sql
CREATE TABLE sector_snapshot (
  sector_code VARCHAR,
  sector_name VARCHAR,
  stock_codes VARCHAR[],
  ingest_at TIMESTAMP
);
```

Insert sample data:

```sql
INSERT INTO sector_snapshot
VALUES
  ('GN_SEMI', 'Semiconductor', ['688981.SH', '603986.SH', '300661.SZ'], now()),
  ('GN_AI', 'Artificial Intelligence', ['688111.SH'], now()),
  ('GN_CLOUD', 'Cloud Computing', ['600570.SH'], now()),
  ('SW_ELEC', 'Electronics', ['002049.SZ'], now()),
  ('SW_COMP', 'Computer Industry', ['002415.SZ'], now()),
  ('SW_MED', 'Healthcare', ['300760.SZ'], now()),
  ('REG_BEIJING', 'Beijing Sector', ['688981.SH'], now()),
  ('INDEX_HS300', 'CSI 300', ['600519.SH'], now()),
  ('INDEX_ZZ500', 'CSI 500', ['300750.SZ'], now()),
  ('ETF_50', 'SSE 50 ETF', ['510050.SH'], now());
```

Check whether the list contains a stock:

```sql
SELECT *
FROM sector_snapshot
WHERE list_contains(stock_codes, '688981.SH');
```

Expand the list into detail rows:

```sql
SELECT
  sector_code,
  sector_name,
  unnest(stock_codes) AS stock_code
FROM sector_snapshot;
```

Usage rules:

- Display "which stocks are in this sector": `VARCHAR[]` is acceptable.
- Query "which sectors contain this stock": use `sector_constituents`.
- Join K-line data, compute returns, or aggregate by sector: use `sector_constituents`.

### 2.4 Partitioned Table

RSDuck partitioned tables use range partition syntax. They do not expose DuckDB Hive directory partition datasets directly. Business code operates on the logical table; physical partitions are created and maintained by RSDuck under `rsduck_internal`.

Create a minute K-line table partitioned by day and retaining 30 partitions:

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

Write only to the logical table:

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
  ('688981.SH', TIMESTAMP '2026-07-10 09:32:00', 50.2, 50.5, 50.1, 50.3, 98000, 4929400, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:33:00', 50.3, 50.6, 50.2, 50.5, 110000, 5555000, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:34:00', 50.5, 50.7, 50.3, 50.4, 88000, 4435200, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:35:00', 50.4, 50.8, 50.4, 50.7, 132000, 6692400, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:36:00', 50.7, 50.9, 50.5, 50.6, 105000, 5313000, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:37:00', 50.6, 50.8, 50.3, 50.4, 96000, 4838400, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:38:00', 50.4, 50.6, 50.2, 50.3, 87000, 4376100, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:39:00', 50.3, 50.5, 50.1, 50.2, 91000, 4568200, 'batch_20260710_001', now()),
  ('688981.SH', TIMESTAMP '2026-07-10 09:40:00', 50.2, 50.4, 50.0, 50.1, 102000, 5110200, 'batch_20260710_001', now());
```

Query the logical table:

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

Show partition status:

```sql
SHOW PARTITIONS FROM kline_1m;
```

Maintenance command:

```sql
CALL rsduck_run_partition_maintenance();
```

If a physical partition is abnormal, mark it first and then repair it:

```sql
CALL rsduck_mark_partition_unavailable(
  'kline_1m',
  '20260710',
  'manual check'
);

CALL rsduck_repair_partition('kline_1m', '20260710');
```

Usage rules:

- The partition key must be `DATE` or `TIMESTAMP`, and must be `NOT NULL`.
- `partition_unit` supports `hour`, `day`, `month`, and `year`.
- External SQL should not directly operate on physical partitions under `rsduck_internal`.
- Use the Web Parquet import entry point for external Parquet files. Do not mix it with the partitioned-table example.

### 2.5 Views

Views are used to persist common queries.

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

Query the view:

```sql
SELECT *
FROM v_sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

Replace the view:

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

### 2.6 Change Table Structure

#### Support Boundary

| Object | Rename column | Change column type | Constraint |
|---|---|---|---|
| Ordinary table | Supported | Supported | Reject the operation when an external dependent view exists. DuckDB converts existing data; the transaction rolls back if conversion fails. |
| Non-partition column of a partitioned table | Supported | Supported | Reject the operation when an external dependent view exists. All active physical partitions change in one transaction; any partition failure rolls back the whole operation. |
| Partition key column | Supported | Not supported | Rename checks external view dependencies, then refreshes partition-routing metadata and the logical entrypoint view. |

Column rename does not convert historical data. A type change must operate on physical data columns; DuckDB performs the conversion validation during DDL execution, and rsduck does not run a redundant pre-scan. The current version supports adding, dropping, renaming, and type changes within the boundary above.

Add a column:

```sql
ALTER TABLE sector_list ADD COLUMN description VARCHAR;
```

Drop a column:

```sql
ALTER TABLE sector_list DROP COLUMN description;
```

Rename a column:

```sql
ALTER TABLE sector_list RENAME COLUMN sector_name TO name;
```

Change a column type:

```sql
ALTER TABLE sector_list ALTER COLUMN constituent_count SET DATA TYPE BIGINT;
```

Check after changing structure:

```sql
DESCRIBE sector_list;
```

Notes:

- Before dropping a column, check whether downstream views, queries, or export tasks depend on it.
- Before changing a column type, check whether existing data can be converted.
- Structural changes should be written to an operation log.

### 2.7 Change Metadata

Use `COMMENT ON` for table and column descriptions.

```sql
COMMENT ON TABLE sector_list IS 'Stock sector master table';
```

```sql
COMMENT ON COLUMN sector_list.category IS 'Sector category: concept, sw_industry, region, index, etc.';
```

Store management metadata in a separate table.

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

Insert sample data:

```sql
INSERT INTO data_catalog
VALUES
  ('sector_list', 'table', 'Sector master table', 'One row represents one stock sector', 'research', 'xtquant', 'scheduled', '0 30 18 * * 1-5', 'low', now()),
  ('sector_constituents', 'table', 'Sector constituent table', 'One row represents one sector constituent stock', 'research', 'xtquant', 'scheduled', '0 30 18 * * 1-5', 'medium', now()),
  ('sector_snapshot', 'table', 'Sector snapshot table', 'Display-oriented snapshot of sector constituent lists', 'research', 'xtquant', 'scheduled', '0 30 18 * * 1-5', 'low', now()),
  ('kline_1m', 'table', 'One-minute quotes', 'Quote data stored by trading minute', 'market-data', 'xtquant', 'scheduled', '*/1 9-15 * * 1-5', 'medium', now()),
  ('kline_1d', 'table', 'Daily quotes', 'Unadjusted quote data stored by trading day', 'market-data', 'xtquant', 'scheduled', '0 0 18 * * 1-5', 'medium', now()),
  ('sector_daily_stats', 'table', 'Sector daily statistics', 'Daily aggregate of sector performance and turnover', 'research', 'rsduck-job', 'scheduled', '0 10 18 * * 1-5', 'low', now()),
  ('v_sector_constituents', 'view', 'Sector constituent view', 'Common view joining sectors and constituents', 'research', 'rsduck', 'manual', NULL, 'low', now()),
  ('sql_example_catalog', 'table', 'SQL example catalog', 'Sample definitions used by the Web SQL page', 'platform', 'rsduck', 'manual', NULL, 'low', now()),
  ('sector_sync_log', 'table', 'Sector synchronization log', 'Records sector constituent synchronization batches and outcomes', 'platform', 'rsduck-job', 'scheduled', '0 30 18 * * 1-5', 'medium', now()),
  ('sql_audit_log', 'table', 'SQL audit log', 'Records high-risk SQL and system operations', 'platform', 'rsduck', 'realtime', NULL, 'high', now());
```

Query the data catalog:

```sql
SELECT *
FROM data_catalog
WHERE object_name = 'sector_constituents';
```

## 3. DML and Queries

### 3.1 Change Records

Change a sector name:

```sql
UPDATE sector_list
SET sector_name = 'Semiconductor'
WHERE sector_code = 'GN_SEMI';
```

Change a sector category:

```sql
UPDATE sector_list
SET category = 'concept'
WHERE sector_code = 'GN_SEMI';
```

Delete one constituent:

```sql
DELETE FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
  AND stock_code = '300661.SZ';
```

Validate after the change:

```sql
SELECT *
FROM sector_constituents
WHERE sector_code = 'GN_SEMI'
ORDER BY stock_code;
```

Execution rules:

- `UPDATE` / `DELETE` must have an explicit `WHERE`.
- Batch changes should preferably run as programmatic tasks.
- High-risk changes should record executor, SQL digest, and affected row count.

### 3.2 Inspect Objects and Structure

Show tables:

```sql
SHOW TABLES;
```

Show columns:

```sql
DESCRIBE sector_list;
```

Show columns with comments:

```sql
SHOW TABLE sector_list;
```

Show sample data:

```sql
SELECT *
FROM sector_list
LIMIT 20;
```

Show view data:

```sql
SELECT *
FROM v_sector_constituents
LIMIT 20;
```

These examples belong on the Web SQL example library home page with low risk.

### 3.3 SQL Example Catalog

Example fields:

- `title`: example name.
- `category`: query, table creation, structural change, data change, data engineering.
- `risk_level`: low, medium, high.
- `entry`: Navicat, Web SQL, backend task.
- `sql_template`: SQL template.
- `params`: replaceable parameters.

Example table:

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

Sample data:

```sql
INSERT INTO sql_example_catalog
VALUES
  ('Query sector constituents', 'query', 'low', 'Web SQL', 'SELECT stock_code FROM sector_constituents WHERE sector_code = $sector_code ORDER BY stock_code', ['sector_code'], now()),
  ('Query sectors for a stock', 'query', 'low', 'Web SQL', 'SELECT sector_code FROM sector_constituents WHERE stock_code = $stock_code ORDER BY sector_code', ['stock_code'], now()),
  ('View sector master table', 'query', 'low', 'Web SQL', 'SELECT * FROM sector_list ORDER BY sector_code LIMIT $limit', ['limit'], now()),
  ('View minute quotes', 'query', 'low', 'Web SQL', 'SELECT * FROM kline_1m WHERE stock_code = $stock_code ORDER BY trade_time DESC LIMIT $limit', ['stock_code', 'limit'], now()),
  ('View partition status', 'query', 'low', 'Web SQL', 'SHOW PARTITIONS FROM kline_1m', [], now()),
  ('Create sector view', 'create', 'medium', 'Web SQL', 'CREATE OR REPLACE VIEW v_sector_constituents AS SELECT s.sector_code, s.sector_name, c.stock_code FROM sector_list s JOIN sector_constituents c ON s.sector_code = c.sector_code', [], now()),
  ('Add sector description', 'schema-change', 'medium', 'Web SQL', 'ALTER TABLE sector_list ADD COLUMN description VARCHAR', [], now()),
  ('Update sector name', 'data-change', 'medium', 'Web SQL', 'UPDATE sector_list SET sector_name = $sector_name WHERE sector_code = $sector_code', ['sector_name', 'sector_code'], now()),
  ('Delete a sector constituent', 'data-change', 'high', 'Web SQL', 'DELETE FROM sector_constituents WHERE sector_code = $sector_code AND stock_code = $stock_code', ['sector_code', 'stock_code'], now()),
  ('Run partition maintenance', 'data-engineering', 'medium', 'Web SQL', 'CALL rsduck_run_partition_maintenance()', [], now());
```

Page rules:

- Low-risk examples can run directly.
- Medium-risk examples show a confirmation dialog.
- High-risk examples require administrator permission and second confirmation.

## 4. Programmatic Scenarios and Code

Runnable Python scripts for this chapter are mapped in [`demo/README.md`](../demo/README.md). Each script uses its own `demo_4_1_` through `demo_5_1_` objects and never changes the tables used in this document.

### 4.1 Full Sector Constituent Sync

Runnable code: [4_1_sector_full_sync.py](../demo/python/4_1_sector_full_sync.py)

Flow:

1. Generate `ingest_batch_id`.
2. Fetch sector list and constituent list.
3. Write to staging tables.
4. Check empty sectors, duplicated constituents, and invalid stock codes.
5. Replace official tables inside a transaction.
6. Write sync log.

Sync log table:

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

Core requirements:

- Do not overwrite old data if a new batch fails validation.
- Every sync must be traceable by batch id.
- Write failure details to `message`.

### 4.2 Incremental Refresh for One Sector

Runnable code: [4_2_sector_incremental_refresh.py](../demo/python/4_2_sector_incremental_refresh.py)

Input parameters:

- `sector_code`
- `source`
- `ingest_batch_id`

Transaction SQL shape:

```sql
BEGIN TRANSACTION;

DELETE FROM sector_constituents
WHERE sector_code = 'GN_SEMI';

INSERT INTO sector_constituents
VALUES
  ('GN_SEMI', '688981.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '603986.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '300661.SZ', 'batch_20260710_002', now()),
  ('GN_SEMI', '002371.SZ', 'batch_20260710_002', now()),
  ('GN_SEMI', '002049.SZ', 'batch_20260710_002', now()),
  ('GN_SEMI', '603501.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '688012.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '688041.SH', 'batch_20260710_002', now()),
  ('GN_SEMI', '300346.SZ', 'batch_20260710_002', now()),
  ('GN_SEMI', '688256.SH', 'batch_20260710_002', now());

UPDATE sector_list
SET constituent_count = 10,
    ingest_batch_id = 'batch_20260710_002',
    ingest_at = now()
WHERE sector_code = 'GN_SEMI';

COMMIT;
```

Failure handling:

- Fetch failure: do not enter the transaction.
- Validation failure: return an error and do not modify official tables.
- Write failure: roll back the transaction.

### 4.3 Sector Quote Aggregation

Runnable code: [4_3_sector_daily_aggregation.py](../demo/python/4_3_sector_daily_aggregation.py)

Result table:

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

Generate statistics by trading day:

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

Query sector ranking for one day:

```sql
SELECT *
FROM sector_daily_stats
WHERE trade_date = DATE '2026-07-10'
ORDER BY avg_pct_chg DESC
LIMIT 20;
```

### 4.4 Parquet Import

Runnable code: [4_4_parquet_import.py](../demo/python/4_4_parquet_import.py). Fixture generator: [4_4_make_parquet_fixture.py](../demo/python/4_4_make_parquet_fixture.py)

Single-file import shape:

```sql
CREATE TABLE imported_daily_quote AS
SELECT *
FROM read_parquet('snapshot/import/daily_quote.parquet');
```

Directory read shape:

```sql
SELECT *
FROM read_parquet('snapshot/import/daily_quote/*.parquet');
```

The import page should:

- Restrict file paths to the allowed root directory.
- Preview columns and row count before import.
- Validate target table names.
- Roll back the whole batch if any file fails.
- Write `data_catalog` after successful import.

### 4.5 Data Quality Checks

Runnable code: [4_5_data_quality_check.py](../demo/python/4_5_data_quality_check.py)

Check sectors without constituents:

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

Check constituent count mismatch:

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

Check invalid stock codes:

```sql
SELECT stock_code
FROM sector_constituents
WHERE NOT regexp_matches(stock_code, '^[0-9]{6}\\.(SH|SZ|BJ)$');
```

Result levels:

- error: block publishing.
- warning: allow publishing but show on page.
- info: log only.

### 4.6 Snapshot and Restore

Runnable code: [4_6_snapshot_restore.py](../demo/python/4_6_snapshot_restore.py)

Snapshot entry points:

- System periodic snapshot.
- Manual Web snapshot.
- Snapshot before service shutdown.

Page fields:

- Snapshot time.
- Trigger type.
- Object count.
- Data file count.
- Manifest verification status.
- Whether it is restorable.

Permission requirements:

- Normal users cannot save or restore snapshots.
- Administrator operations require audit logs.
- Show target snapshot manifest information before restore.

### 4.7 Privilege and Audit

Runnable code: [4_7_permission_audit.py](../demo/python/4_7_permission_audit.py)

Create an analyst role:

```sql
CREATE ROLE analyst;
```

Grant read access to sector tables:

```sql
GRANT SELECT ON TABLE sector_list TO ROLE analyst;
GRANT SELECT ON TABLE sector_constituents TO ROLE analyst;
```

Audit log fields:

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

Record scope:

- DDL: `CREATE`, `ALTER`, `DROP`
- DML: `INSERT`, `UPDATE`, `DELETE`
- System operations: import, snapshot, restore, privilege changes

### 4.8 Continuous Writes

Runnable code:

- HTTP: [4_8_http_continuous_write.py](../demo/python/4_8_http_continuous_write.py)
- MySQL wire: [4_8_mysql_continuous_write.py](../demo/python/4_8_mysql_continuous_write.py)

The two scripts use the same table shape, generated data, batch size, write interval, and metrics to validate continuous writes through HTTP and MySQL wire. HTTP uses `demo_4_8_http_quote_ticks` and MySQL wire uses `demo_4_8_mysql_quote_ticks`, so they can run together. Run them separately when comparing performance to avoid contention for the same write worker.

The HTTP variant sends multi-row `INSERT` statements through the Web API:

```powershell
python demo/python/4_8_http_continuous_write.py
```

The MySQL wire variant uses one PyMySQL connection and parameterized `executemany` batches:

```powershell
pip install PyMySQL
python demo/python/4_8_mysql_continuous_write.py
```

Shared options:

- `--batch-size 100`: write 100 rows in each `INSERT`.
- `--interval-ms 100`: submit one batch every 100 milliseconds.
- `--duration 0`: run until manually stopped.

The MySQL variant also accepts `--host`, `--port`, and `--database`; it connects to `127.0.0.1:13306` and `main` by default. Both scripts run for 60 seconds by default, create their table only when it does not exist, and never delete accumulated data. They report both batch rows and total table rows at completion. Drop the corresponding table manually when cleanup is required.

## 5. Web Implementation

### 5.1 Web Console API Verification

Runnable code: [5_1_web_console_api_smoke.py](../demo/python/5_1_web_console_api_smoke.py)

SQL example library:

- Filter by risk level and operation type.
- Generate SQL after parameter input.
- Low-risk examples can run directly; high-risk examples require confirmation.

Data object management:

- Show tables, views, columns, comments, and data catalog information.
- Show row-count overview and latest update time.
- Metadata changes should use controlled forms, not direct system-table editing.

Data tasks:

- Sector sync.
- Parquet import.
- Partitioned table maintenance.
- Data quality checks.
- Snapshot save and restore.

Each task should at least show: status, start time, finish time, batch id, affected row count, and failure reason.

## 6. References

- DuckDB `CREATE TABLE`: https://duckdb.org/docs/current/sql/statements/create_table
- DuckDB `ALTER TABLE`: https://duckdb.org/docs/current/sql/statements/alter_table
- DuckDB `CREATE VIEW`: https://duckdb.org/docs/current/sql/statements/create_view
- DuckDB `COMMENT ON`: https://duckdb.org/docs/current/sql/statements/comment_on
- DuckDB `INSERT`: https://duckdb.org/docs/current/sql/statements/insert
- DuckDB `UPDATE`: https://duckdb.org/docs/current/sql/statements/update
- DuckDB `DELETE`: https://duckdb.org/docs/current/sql/statements/delete

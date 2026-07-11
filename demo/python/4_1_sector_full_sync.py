from common import assert_equal, build_parser, client_from_args


TABLES = [
    "demo_4_1_sector_constituents_staging",
    "demo_4_1_sector_list_staging",
    "demo_4_1_sector_constituents",
    "demo_4_1_sector_list",
    "demo_4_1_sync_log",
]


def cleanup(client):
    for table in TABLES:
        client.sql("DROP TABLE IF EXISTS %s" % table)


def main():
    args = build_parser("Section 4.1: full sector synchronization").parse_args()
    client = client_from_args(args)
    cleanup(client)
    client.sql("CREATE TABLE demo_4_1_sector_list(sector_code VARCHAR PRIMARY KEY, sector_name VARCHAR, constituent_count INTEGER, ingest_batch_id VARCHAR)")
    client.sql("CREATE TABLE demo_4_1_sector_constituents(sector_code VARCHAR, stock_code VARCHAR, ingest_batch_id VARCHAR)")
    client.sql("CREATE TABLE demo_4_1_sector_list_staging(sector_code VARCHAR, sector_name VARCHAR, constituent_count INTEGER, ingest_batch_id VARCHAR)")
    client.sql("CREATE TABLE demo_4_1_sector_constituents_staging(sector_code VARCHAR, stock_code VARCHAR, ingest_batch_id VARCHAR)")
    client.sql("CREATE TABLE demo_4_1_sync_log(ingest_batch_id VARCHAR, source VARCHAR, sector_count INTEGER, constituent_count INTEGER, status VARCHAR, message VARCHAR)")
    client.sql("INSERT INTO demo_4_1_sector_list_staging VALUES ('DEMO_SEMI', 'Semiconductor', 5, 'demo_4_1_001'), ('DEMO_AI', 'Artificial Intelligence', 5, 'demo_4_1_001')")
    client.sql("INSERT INTO demo_4_1_sector_constituents_staging VALUES ('DEMO_SEMI', '688981.SH', 'demo_4_1_001'), ('DEMO_SEMI', '603986.SH', 'demo_4_1_001'), ('DEMO_SEMI', '300661.SZ', 'demo_4_1_001'), ('DEMO_SEMI', '002371.SZ', 'demo_4_1_001'), ('DEMO_SEMI', '002049.SZ', 'demo_4_1_001'), ('DEMO_AI', '688111.SH', 'demo_4_1_001'), ('DEMO_AI', '002230.SZ', 'demo_4_1_001'), ('DEMO_AI', '300308.SZ', 'demo_4_1_001'), ('DEMO_AI', '002415.SZ', 'demo_4_1_001'), ('DEMO_AI', '688256.SH', 'demo_4_1_001')")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_1_sector_list_staging WHERE constituent_count = 0"), 0, "empty_sector_rows")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM (SELECT sector_code, stock_code FROM demo_4_1_sector_constituents_staging GROUP BY sector_code, stock_code HAVING COUNT(*) > 1) duplicates"), 0, "duplicate_constituent_rows")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_1_sector_constituents_staging WHERE NOT regexp_matches(stock_code, '^[0-9]{6}\\.(SH|SZ|BJ)$')"), 0, "invalid_stock_code_rows")
    client.sql("BEGIN TRANSACTION")
    client.sql("INSERT INTO demo_4_1_sector_list SELECT * FROM demo_4_1_sector_list_staging")
    client.sql("INSERT INTO demo_4_1_sector_constituents SELECT * FROM demo_4_1_sector_constituents_staging")
    client.sql("INSERT INTO demo_4_1_sync_log VALUES ('demo_4_1_001', 'demo', 2, 10, 'success', '')")
    client.sql("COMMIT")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_1_sector_list"), 2, "sector_count")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_1_sector_constituents"), 10, "constituent_count")
    if args.cleanup:
        cleanup(client)


if __name__ == "__main__":
    main()

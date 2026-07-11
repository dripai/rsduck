from common import assert_equal, build_parser, client_from_args


TABLES = ["demo_4_2_sector_constituents", "demo_4_2_sector_list"]


def cleanup(client):
    for table in TABLES:
        client.sql("DROP TABLE IF EXISTS %s" % table)


def main():
    args = build_parser("Section 4.2: incremental sector refresh").parse_args()
    client = client_from_args(args)
    cleanup(client)
    client.sql("CREATE TABLE demo_4_2_sector_list(sector_code VARCHAR PRIMARY KEY, constituent_count INTEGER, ingest_batch_id VARCHAR)")
    client.sql("CREATE TABLE demo_4_2_sector_constituents(sector_code VARCHAR, stock_code VARCHAR, ingest_batch_id VARCHAR)")
    client.sql("INSERT INTO demo_4_2_sector_list VALUES ('DEMO_SEMI', 2, 'demo_4_2_001')")
    client.sql("INSERT INTO demo_4_2_sector_constituents VALUES ('DEMO_SEMI', '688981.SH', 'demo_4_2_001'), ('DEMO_SEMI', '603986.SH', 'demo_4_2_001')")
    client.sql("BEGIN TRANSACTION")
    client.sql("DELETE FROM demo_4_2_sector_constituents WHERE sector_code = 'DEMO_SEMI'")
    client.sql("INSERT INTO demo_4_2_sector_constituents VALUES ('DEMO_SEMI', '688981.SH', 'demo_4_2_002'), ('DEMO_SEMI', '603986.SH', 'demo_4_2_002'), ('DEMO_SEMI', '300661.SZ', 'demo_4_2_002'), ('DEMO_SEMI', '002371.SZ', 'demo_4_2_002'), ('DEMO_SEMI', '002049.SZ', 'demo_4_2_002'), ('DEMO_SEMI', '603501.SH', 'demo_4_2_002'), ('DEMO_SEMI', '688012.SH', 'demo_4_2_002'), ('DEMO_SEMI', '688041.SH', 'demo_4_2_002'), ('DEMO_SEMI', '300346.SZ', 'demo_4_2_002'), ('DEMO_SEMI', '688256.SH', 'demo_4_2_002')")
    client.sql("UPDATE demo_4_2_sector_list SET constituent_count = 10, ingest_batch_id = 'demo_4_2_002' WHERE sector_code = 'DEMO_SEMI'")
    client.sql("COMMIT")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_2_sector_constituents WHERE sector_code = 'DEMO_SEMI'"), 10, "refreshed_constituent_count")
    if args.cleanup:
        cleanup(client)


if __name__ == "__main__":
    main()

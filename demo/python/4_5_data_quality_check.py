from common import assert_equal, build_parser, client_from_args


TABLES = ["demo_4_5_sector_constituents", "demo_4_5_sector_list"]


def cleanup(client):
    for table in TABLES:
        client.sql("DROP TABLE IF EXISTS %s" % table)


def main():
    args = build_parser("Section 4.5: data quality checks").parse_args()
    client = client_from_args(args)
    cleanup(client)
    client.sql("CREATE TABLE demo_4_5_sector_list(sector_code VARCHAR, constituent_count INTEGER)")
    client.sql("CREATE TABLE demo_4_5_sector_constituents(sector_code VARCHAR, stock_code VARCHAR)")
    client.sql("INSERT INTO demo_4_5_sector_list VALUES ('DEMO_EMPTY', 0), ('DEMO_MISMATCH', 3), ('DEMO_VALID', 1)")
    client.sql("INSERT INTO demo_4_5_sector_constituents VALUES ('DEMO_MISMATCH', '688981.SH'), ('DEMO_VALID', '603986.SH'), ('DEMO_VALID', 'BAD_CODE')")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM (SELECT s.sector_code FROM demo_4_5_sector_list s LEFT JOIN demo_4_5_sector_constituents c ON c.sector_code = s.sector_code GROUP BY s.sector_code HAVING COUNT(c.stock_code) = 0) empty_sectors"), 1, "empty_sector_rows")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM (SELECT s.sector_code FROM demo_4_5_sector_list s LEFT JOIN demo_4_5_sector_constituents c ON c.sector_code = s.sector_code GROUP BY s.sector_code, s.constituent_count HAVING s.constituent_count <> COUNT(c.stock_code)) mismatches"), 2, "count_mismatch_rows")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_5_sector_constituents WHERE NOT regexp_matches(stock_code, '^[0-9]{6}\\.(SH|SZ|BJ)$')"), 1, "invalid_stock_code_rows")
    if args.cleanup:
        cleanup(client)


if __name__ == "__main__":
    main()

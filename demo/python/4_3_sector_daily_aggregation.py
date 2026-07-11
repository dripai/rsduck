from common import assert_equal, build_parser, client_from_args


TABLES = ["demo_4_3_sector_daily_stats", "demo_4_3_kline_1d", "demo_4_3_sector_constituents"]


def cleanup(client):
    for table in TABLES:
        client.sql("DROP TABLE IF EXISTS %s" % table)


def main():
    args = build_parser("Section 4.3: sector daily aggregation").parse_args()
    client = client_from_args(args)
    cleanup(client)
    client.sql("CREATE TABLE demo_4_3_sector_constituents(sector_code VARCHAR, stock_code VARCHAR)")
    client.sql("CREATE TABLE demo_4_3_kline_1d(stock_code VARCHAR, trade_date DATE, pct_chg DOUBLE, amount DOUBLE)")
    client.sql("CREATE TABLE demo_4_3_sector_daily_stats(sector_code VARCHAR, trade_date DATE, stock_count INTEGER, up_count INTEGER, down_count INTEGER, avg_pct_chg DOUBLE, total_amount DOUBLE, ingest_batch_id VARCHAR, ingest_at TIMESTAMP)")
    client.sql("INSERT INTO demo_4_3_sector_constituents VALUES ('DEMO_SEMI', '688981.SH'), ('DEMO_SEMI', '603986.SH'), ('DEMO_AI', '688111.SH')")
    client.sql("INSERT INTO demo_4_3_kline_1d VALUES ('688981.SH', DATE '2026-07-10', 2.1, 1000), ('603986.SH', DATE '2026-07-10', -1.0, 2000), ('688111.SH', DATE '2026-07-10', 3.0, 1500)")
    client.sql("INSERT INTO demo_4_3_sector_daily_stats SELECT c.sector_code, k.trade_date, COUNT(*), SUM(CASE WHEN k.pct_chg > 0 THEN 1 ELSE 0 END), SUM(CASE WHEN k.pct_chg < 0 THEN 1 ELSE 0 END), AVG(k.pct_chg), SUM(k.amount), 'demo_4_3_001', now() FROM demo_4_3_sector_constituents c JOIN demo_4_3_kline_1d k ON k.stock_code = c.stock_code GROUP BY c.sector_code, k.trade_date")
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM demo_4_3_sector_daily_stats"), 2, "daily_stat_rows")
    if args.cleanup:
        cleanup(client)


if __name__ == "__main__":
    main()

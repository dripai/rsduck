from common import DemoError, build_parser, client_from_args


def main():
    parser = build_parser("Section 4.4: Web Parquet import")
    parser.add_argument("--source", required=True, help="path relative to web.parquet_import_root")
    parser.add_argument("--table", default="demo_4_4_daily_quote")
    args = parser.parse_args()
    client = client_from_args(args)
    client.sql("DROP TABLE IF EXISTS %s" % args.table)
    result = client.import_parquet(args.source, "main", args.table)
    print(result["msg"])
    rows = client.scalar_int("SELECT COUNT(*) FROM %s" % args.table)
    if rows < 1:
        raise DemoError("Parquet import produced no rows")
    print("verified imported_rows=%s" % rows)
    if args.cleanup:
        client.sql("DROP TABLE IF EXISTS %s" % args.table)


if __name__ == "__main__":
    main()

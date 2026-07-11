from common import assert_equal, build_parser, client_from_args


TABLE = "demo_5_1_console_probe"


def main():
    args = build_parser("Section 5.1: Web console API smoke test").parse_args()
    client = client_from_args(args)
    session = client.request("/session", method="GET")
    if not session.get("authenticated") or session.get("username") != args.username:
        raise RuntimeError("Web session was not established")
    client.sql("DROP TABLE IF EXISTS %s" % TABLE)
    client.sql("CREATE TABLE %s(id INTEGER, label VARCHAR)" % TABLE)
    client.sql("COMMENT ON COLUMN %s.label IS 'demo label'" % TABLE)
    client.sql("INSERT INTO %s VALUES (1, 'ok')" % TABLE)
    assert_equal(client.scalar_int("SELECT COUNT(*) FROM %s" % TABLE), 1, "console_query_rows")
    result = client.sql("SHOW TABLE %s" % TABLE)
    comments = [row[6] for row in result.get("rows", []) if row[0] == "label"]
    assert_equal(comments, ["demo label"], "show_table_comment")
    if args.cleanup:
        client.sql("DROP TABLE IF EXISTS %s" % TABLE)


if __name__ == "__main__":
    main()

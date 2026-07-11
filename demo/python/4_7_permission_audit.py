from common import DEMO_PASSWORD, DemoClient, DemoError, assert_equal, build_parser, client_from_args


ROLE = "demo_4_7_reader"
USER = "demo_4_7_user"
TABLE = "demo_4_7_quotes"


def cleanup(client):
    for statement in [
        "REVOKE ROLE %s FROM %s" % (ROLE, USER),
        "REVOKE SELECT ON TABLE %s FROM ROLE %s" % (TABLE, ROLE),
    ]:
        try:
            client.sql(statement)
        except DemoError as exc:
            if "does not exist" not in str(exc).lower():
                raise
    client.sql("DROP USER IF EXISTS %s" % USER)
    client.sql("DROP ROLE IF EXISTS %s CASCADE" % ROLE)
    client.sql("DROP TABLE IF EXISTS %s" % TABLE)


def main():
    args = build_parser("Section 4.7: role, privilege, and audit workflow").parse_args()
    admin = client_from_args(args)
    cleanup(admin)
    admin.sql("CREATE TABLE %s(code VARCHAR, close DOUBLE)" % TABLE)
    admin.sql("INSERT INTO %s VALUES ('688981.SH', 50.2)" % TABLE)
    admin.sql("CREATE ROLE %s" % ROLE)
    admin.sql("CREATE USER %s PASSWORD='%s'" % (USER, DEMO_PASSWORD))
    admin.sql("GRANT SELECT ON TABLE %s TO ROLE %s" % (TABLE, ROLE))
    admin.sql("GRANT ROLE %s TO %s" % (ROLE, USER))
    reader = DemoClient(args.url, USER, DEMO_PASSWORD, args.timeout)
    assert_equal(reader.scalar_int("SELECT COUNT(*) FROM %s" % TABLE), 1, "reader_select_rows")
    reader.expect_sql_failure("INSERT INTO %s VALUES ('603986.SH', 100)" % TABLE, "permission denied")
    if args.cleanup:
        cleanup(admin)


if __name__ == "__main__":
    main()

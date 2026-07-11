import argparse
import time
from datetime import datetime

from common import DEMO_PASSWORD, add_mysql_connection_args
from continuous_write_common import (
    add_continuous_write_args,
    build_rows,
    create_table_sql,
    new_batch_id,
    print_report,
    table_name,
    validate_continuous_write_args,
)


TABLE = table_name("mysql")
INSERT_SQL = (
    "INSERT INTO %s(seq, stock_code, event_time, close, volume, batch_id) "
    "VALUES (%%s, %%s, %%s, %%s, %%s, %%s)" % TABLE
)


def main():
    try:
        import pymysql
    except ImportError as exc:
        raise SystemExit("install MySQL demo dependency first: pip install PyMySQL") from exc

    parser = argparse.ArgumentParser(description="Section 4.8: continuous MySQL wire writes")
    add_mysql_connection_args(parser, include_cleanup=False)
    add_continuous_write_args(parser)
    args = parser.parse_args()
    validate_continuous_write_args(args)

    connection = pymysql.connect(
        host=args.host,
        port=args.port,
        user=args.username,
        password=DEMO_PASSWORD,
        database=args.database,
        autocommit=True,
        charset="utf8mb4",
        connect_timeout=args.timeout,
        read_timeout=args.timeout,
        write_timeout=args.timeout,
    )
    try:
        with connection.cursor() as cursor:
            cursor.execute(create_table_sql(TABLE, if_not_exists=True))

            batch_id = new_batch_id("mysql")
            base_time = datetime.now()
            started_at = time.monotonic()
            next_report_at = started_at + args.report_interval
            sequence = rows = batches = errors = 0
            latency_sum = max_latency = 0.0
            print(
                "protocol=mysql host=%s:%d database=%s table=%s batch_size=%d interval_ms=%.0f duration=%.1fs"
                % (
                    args.host,
                    args.port,
                    args.database,
                    TABLE,
                    args.batch_size,
                    args.interval_ms,
                    args.duration,
                )
            )

            try:
                while args.duration == 0 or time.monotonic() - started_at < args.duration:
                    batch_started = time.monotonic()
                    try:
                        batch = build_rows(sequence, args.batch_size, args.symbols, batch_id, base_time)
                        cursor.executemany(INSERT_SQL, batch)
                        elapsed = time.monotonic() - batch_started
                        rows += args.batch_size
                        batches += 1
                        latency_sum += elapsed
                        max_latency = max(max_latency, elapsed)
                        sequence += args.batch_size
                    except pymysql.MySQLError as exc:
                        errors += 1
                        print("write failed: %s" % exc)

                    now = time.monotonic()
                    if now >= next_report_at:
                        print_report(rows, batches, errors, latency_sum, max_latency, started_at)
                        next_report_at = now + args.report_interval
                    sleep_seconds = args.interval_ms / 1000.0 - (time.monotonic() - batch_started)
                    if sleep_seconds > 0:
                        time.sleep(sleep_seconds)
            except KeyboardInterrupt:
                print("stopped by Ctrl+C")

            print_report(rows, batches, errors, latency_sum, max_latency, started_at)
            cursor.execute("SELECT COUNT(*) FROM %s WHERE batch_id = %%s" % TABLE, (batch_id,))
            persisted_rows = cursor.fetchone()[0]
            if persisted_rows != rows:
                raise RuntimeError("persisted rows mismatch: expected %d, got %d" % (rows, persisted_rows))
            cursor.execute("SELECT COUNT(*) FROM %s" % TABLE)
            total_rows = cursor.fetchone()[0]
            print("verified protocol=mysql batch_rows=%d table_rows=%d" % (persisted_rows, total_rows))
    finally:
        connection.close()


if __name__ == "__main__":
    main()

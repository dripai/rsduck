import time
from datetime import datetime

from common import DemoError, build_parser, client_from_args
from continuous_write_common import (
    add_continuous_write_args,
    build_insert_sql,
    build_rows,
    create_table_sql,
    new_batch_id,
    print_report,
    table_name,
    validate_continuous_write_args,
)


TABLE = table_name("http")


def main():
    parser = build_parser("Section 4.8: continuous HTTP writes", include_cleanup=False)
    add_continuous_write_args(parser)
    args = parser.parse_args()
    validate_continuous_write_args(args)

    client = client_from_args(args)
    client.sql(create_table_sql(TABLE, if_not_exists=True))

    batch_id = new_batch_id("http")
    base_time = datetime.now()
    started_at = time.monotonic()
    next_report_at = started_at + args.report_interval
    sequence = rows = batches = errors = 0
    latency_sum = max_latency = 0.0
    print(
        "protocol=http target=%s table=%s batch_size=%d interval_ms=%.0f duration=%.1fs"
        % (args.url, TABLE, args.batch_size, args.interval_ms, args.duration)
    )

    try:
        while args.duration == 0 or time.monotonic() - started_at < args.duration:
            batch_started = time.monotonic()
            try:
                batch = build_rows(sequence, args.batch_size, args.symbols, batch_id, base_time)
                client.sql(build_insert_sql(TABLE, batch), page_size=1, echo=False)
                elapsed = time.monotonic() - batch_started
                rows += args.batch_size
                batches += 1
                latency_sum += elapsed
                max_latency = max(max_latency, elapsed)
                sequence += args.batch_size
            except DemoError as exc:
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
    persisted_rows = client.scalar_int("SELECT COUNT(*) FROM %s WHERE batch_id = '%s'" % (TABLE, batch_id))
    if persisted_rows != rows:
        raise DemoError("persisted rows mismatch: expected %d, got %d" % (rows, persisted_rows))
    total_rows = client.scalar_int("SELECT COUNT(*) FROM %s" % TABLE)
    print("verified protocol=http batch_rows=%d table_rows=%d" % (persisted_rows, total_rows))


if __name__ == "__main__":
    main()

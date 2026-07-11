import time
from datetime import datetime, timedelta


def table_name(protocol):
    return "demo_4_8_%s_quote_ticks" % protocol


def create_table_sql(table, if_not_exists=False):
    if_not_exists_sql = " IF NOT EXISTS" if if_not_exists else ""
    return (
        "CREATE TABLE%s %s("
        "seq BIGINT, stock_code VARCHAR, event_time TIMESTAMP, "
        "close DOUBLE, volume BIGINT, batch_id VARCHAR)" % (if_not_exists_sql, table)
    )


def add_continuous_write_args(parser):
    parser.add_argument("--duration", type=float, default=60.0, help="seconds to run; 0 runs until Ctrl+C")
    parser.add_argument("--interval-ms", type=float, default=1000.0, help="delay between batches")
    parser.add_argument("--batch-size", type=int, default=10, help="rows per batch")
    parser.add_argument("--symbols", type=int, default=10, help="number of generated stock codes")
    parser.add_argument("--report-interval", type=float, default=5.0, help="seconds between reports")


def validate_continuous_write_args(args):
    if args.duration < 0 or args.interval_ms < 0:
        raise SystemExit("duration and interval must be non-negative")
    if args.batch_size < 1 or args.symbols < 1 or args.report_interval <= 0 or args.timeout <= 0:
        raise SystemExit("batch-size, symbols, report-interval, and timeout must be positive")


def new_batch_id(protocol):
    return "demo_4_8_%s_%s" % (protocol, datetime.now().strftime("%Y%m%d_%H%M%S"))


def build_rows(sequence_start, batch_size, symbols, batch_id, base_time):
    rows = []
    for offset in range(batch_size):
        sequence = sequence_start + offset
        rows.append(
            (
                sequence,
                "6%05d.SH" % (sequence % symbols),
                base_time + timedelta(milliseconds=sequence),
                10.0 + (sequence % 1000) / 100.0,
                1000 + (sequence % 100000),
                batch_id,
            )
        )
    return rows


def sql_literal(value):
    if isinstance(value, datetime):
        return "'%s'" % value.strftime("%Y-%m-%d %H:%M:%S.%f")
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    return str(value)


def build_insert_sql(table, rows):
    values = ["(" + ", ".join(sql_literal(value) for value in row) + ")" for row in rows]
    return (
        "INSERT INTO %s(seq, stock_code, event_time, close, volume, batch_id) VALUES " % table
        + ", ".join(values)
    )


def print_report(rows, batches, errors, latency_sum, max_latency, started_at):
    elapsed = max(time.monotonic() - started_at, 0.001)
    average_ms = latency_sum * 1000.0 / batches if batches else 0.0
    print(
        "%s rows=%d batches=%d errors=%d rows/s=%.1f avg_batch=%.1fms max_batch=%.1fms"
        % (
            time.strftime("%H:%M:%S"),
            rows,
            batches,
            errors,
            rows / elapsed,
            average_ms,
            max_latency * 1000.0,
        )
    )

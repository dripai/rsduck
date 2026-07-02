import argparse
import itertools
import json
import random
import threading
import time
from datetime import datetime, timedelta
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


QUERY_SQL = (
    "SELECT COUNT(*) AS total FROM kline_day",
    "SELECT * FROM kline_day ORDER BY bar_time DESC LIMIT 20",
    "SELECT code, close, bar_time FROM kline_day ORDER BY bar_time DESC LIMIT 1",
    (
        "SELECT code, COUNT(*) AS rows, AVG(close) AS avg_close "
        "FROM kline_day GROUP BY code ORDER BY rows DESC LIMIT 10"
    ),
)


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.write_rows = 0
        self.write_batches = 0
        self.write_errors = 0
        self.query_count = 0
        self.query_errors = 0
        self.write_latency_sum = 0.0
        self.query_latency_sum = 0.0
        self.max_write_latency = 0.0
        self.max_query_latency = 0.0

    def record_write(self, rows, elapsed):
        with self.lock:
            self.write_rows += rows
            self.write_batches += 1
            self.write_latency_sum += elapsed
            self.max_write_latency = max(self.max_write_latency, elapsed)

    def record_write_error(self):
        with self.lock:
            self.write_errors += 1

    def record_query(self, elapsed):
        with self.lock:
            self.query_count += 1
            self.query_latency_sum += elapsed
            self.max_query_latency = max(self.max_query_latency, elapsed)

    def record_query_error(self):
        with self.lock:
            self.query_errors += 1

    def snapshot(self):
        with self.lock:
            avg_write_ms = 0.0
            if self.write_batches:
                avg_write_ms = self.write_latency_sum * 1000.0 / self.write_batches

            avg_query_ms = 0.0
            if self.query_count:
                avg_query_ms = self.query_latency_sum * 1000.0 / self.query_count

            return {
                "write_rows": self.write_rows,
                "write_batches": self.write_batches,
                "write_errors": self.write_errors,
                "query_count": self.query_count,
                "query_errors": self.query_errors,
                "avg_write_ms": avg_write_ms,
                "avg_query_ms": avg_query_ms,
                "max_write_ms": self.max_write_latency * 1000.0,
                "max_query_ms": self.max_query_latency * 1000.0,
            }


def sql_literal(value):
    if value is None:
        return "NULL"
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    return str(value)


def build_row(seq, symbols, base_time):
    code = "6%05d" % (seq % symbols)
    bar_time = base_time + timedelta(microseconds=seq)
    close = round(random.uniform(5.0, 120.0), 2)
    open_price = round(close + random.uniform(-1.5, 1.5), 2)
    high = round(max(open_price, close) + random.uniform(0.0, 2.0), 2)
    low = round(min(open_price, close) - random.uniform(0.0, 2.0), 2)
    volume = random.randint(1000, 500000)
    return (
        code,
        bar_time.strftime("%Y-%m-%d %H:%M:%S.%f"),
        open_price,
        high,
        low,
        close,
        volume,
    )


def build_insert_sql(rows):
    values = []
    for row in rows:
        values.append("(" + ", ".join(sql_literal(value) for value in row) + ")")

    return (
        "INSERT INTO kline_day "
        "(code, bar_time, open, high, low, close, volume) VALUES "
        + ", ".join(values)
    )


def post_sql(base_url, sql, page, page_size, timeout):
    payload = {
        "sql": sql,
        "page": page,
        "page_size": page_size,
    }
    data = json.dumps(payload).encode("utf-8")
    request = Request(
        base_url.rstrip("/") + "/sql",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(request, timeout=timeout) as response:
        body = response.read().decode("utf-8")
    return json.loads(body)


def write_loop(args, stop, stats):
    counter = itertools.count()
    base_time = datetime.now()
    while not stop.is_set():
        started = time.time()
        rows = [build_row(next(counter), args.symbols, base_time) for _ in range(args.write_batch)]
        sql = build_insert_sql(rows)

        try:
            result = post_sql(args.url, sql, 0, 1, args.timeout)
            elapsed = time.time() - started
            if result.get("success"):
                stats.record_write(len(rows), elapsed)
            else:
                stats.record_write_error()
                print_error("write failed", result.get("msg"), args.print_sql_on_error, sql)
        except (HTTPError, URLError, TimeoutError, OSError, ValueError) as exc:
            stats.record_write_error()
            print_error("write failed", str(exc), args.print_sql_on_error, sql)

        sleep_left = args.write_interval - (time.time() - started)
        if sleep_left > 0:
            stop.wait(sleep_left)


def query_loop(worker_id, args, stop, stats):
    randomizer = random.Random(time.time() + worker_id)
    while not stop.is_set():
        sql = randomizer.choice(QUERY_SQL)
        started = time.time()

        try:
            result = post_sql(args.url, sql, 0, args.page_size, args.timeout)
            elapsed = time.time() - started
            if result.get("success"):
                stats.record_query(elapsed)
            else:
                stats.record_query_error()
                print_error(
                    "query-%d failed" % worker_id,
                    result.get("msg"),
                    args.print_sql_on_error,
                    sql,
                )
        except (HTTPError, URLError, TimeoutError, OSError, ValueError) as exc:
            stats.record_query_error()
            print_error("query-%d failed" % worker_id, str(exc), args.print_sql_on_error, sql)

        stop.wait(args.query_interval)


def print_error(prefix, message, print_sql, sql):
    print("%s: %s" % (prefix, message or "unknown error"))
    if print_sql:
        print(sql)


def report_loop(args, stop, stats, started_at):
    while not stop.wait(args.report_interval):
        print_report(stats, started_at)


def print_report(stats, started_at):
    current = stats.snapshot()
    elapsed = max(0.001, time.time() - started_at)
    write_rate = current["write_rows"] / elapsed
    query_rate = current["query_count"] / elapsed
    print(
        (
            "%s rows=%d batches=%d write_err=%d q=%d q_err=%d "
            "write/s=%.1f q/s=%.1f avg_write=%.1fms avg_q=%.1fms "
            "max_write=%.1fms max_q=%.1fms"
        )
        % (
            time.strftime("%H:%M:%S"),
            current["write_rows"],
            current["write_batches"],
            current["write_errors"],
            current["query_count"],
            current["query_errors"],
            write_rate,
            query_rate,
            current["avg_write_ms"],
            current["avg_query_ms"],
            current["max_write_ms"],
            current["max_query_ms"],
        )
    )


def parse_args():
    parser = argparse.ArgumentParser(
        description="HTTP load test for rsduck: one continuous writer plus concurrent query workers."
    )
    parser.add_argument("--url", default="http://127.0.0.1:8080", help="rsduck web base URL")
    parser.add_argument("--write-interval", type=float, default=0.5, help="seconds between write batches")
    parser.add_argument("--write-batch", type=int, default=10, help="rows per write batch")
    parser.add_argument("--query-workers", type=int, default=4, help="concurrent query worker count")
    parser.add_argument("--query-interval", type=float, default=0.2, help="seconds between each worker query")
    parser.add_argument("--symbols", type=int, default=100, help="number of generated stock codes")
    parser.add_argument("--page-size", type=int, default=1000, help="page_size sent to /sql for query requests")
    parser.add_argument("--timeout", type=float, default=10.0, help="HTTP timeout in seconds")
    parser.add_argument("--report-interval", type=float, default=5.0, help="seconds between progress reports")
    parser.add_argument("--duration", type=float, default=0.0, help="stop after seconds; 0 means run until Ctrl+C")
    parser.add_argument("--print-sql-on-error", action="store_true", help="print failed SQL text")
    return parser.parse_args()


def validate_args(args):
    if args.write_batch < 1:
        raise SystemExit("--write-batch must be >= 1")
    if args.write_interval < 0:
        raise SystemExit("--write-interval must be >= 0")
    if args.query_workers < 1:
        raise SystemExit("--query-workers must be >= 1")
    if args.query_interval < 0:
        raise SystemExit("--query-interval must be >= 0")
    if args.symbols < 1:
        raise SystemExit("--symbols must be >= 1")
    if args.page_size < 1:
        raise SystemExit("--page-size must be >= 1")
    if args.timeout <= 0:
        raise SystemExit("--timeout must be > 0")
    if args.report_interval <= 0:
        raise SystemExit("--report-interval must be > 0")


def main():
    args = parse_args()
    validate_args(args)

    stop = threading.Event()
    stats = Stats()
    started_at = time.time()

    threads = [
        threading.Thread(target=write_loop, args=(args, stop, stats), name="rsduck-writer"),
        threading.Thread(target=report_loop, args=(args, stop, stats, started_at), name="rsduck-reporter"),
    ]

    for worker_id in range(args.query_workers):
        threads.append(
            threading.Thread(
                target=query_loop,
                args=(worker_id + 1, args, stop, stats),
                name="rsduck-query-%d" % (worker_id + 1),
            )
        )

    print(
        (
            "target=%s write_interval=%.3fs write_batch=%d "
            "query_workers=%d query_interval=%.3fs page_size=%d"
        )
        % (
            args.url,
            args.write_interval,
            args.write_batch,
            args.query_workers,
            args.query_interval,
            args.page_size,
        )
    )
    print("press Ctrl+C to stop")

    for thread in threads:
        thread.daemon = True
        thread.start()

    try:
        if args.duration > 0:
            stop.wait(args.duration)
        else:
            while not stop.wait(0.5):
                pass
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        for thread in threads:
            thread.join(timeout=2.0)
        print_report(stats, started_at)
        print("stopped")


if __name__ == "__main__":
    main()

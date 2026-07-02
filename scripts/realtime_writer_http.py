import argparse
import json
import random
import time
from datetime import datetime
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def build_row(symbol_count: int) -> tuple:
    code = f"6{random.randint(0, symbol_count - 1):05d}"
    base = random.uniform(4.0, 60.0)
    open_price = round(base, 2)
    high = round(base * random.uniform(1.000, 1.025), 2)
    low = round(base * random.uniform(0.975, 1.000), 2)
    close = round(random.uniform(low, high), 2)
    volume = random.randint(10_000, 800_000)
    return code, datetime.now().strftime("%Y-%m-%d %H:%M:%S.%f"), open_price, high, low, close, volume


def sql_literal(value) -> str:
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    return str(value)


def build_insert_sql(rows) -> str:
    values = []
    for row in rows:
        values.append("(" + ", ".join(sql_literal(v) for v in row) + ")")
    return """
        INSERT INTO kline_day
        (code, bar_time, open, high, low, close, volume)
        VALUES
    """ + ",\n".join(values)


def post_sql(base_url: str, sql: str) -> dict:
    payload = json.dumps({"sql": sql, "page": 0, "page_size": 100}).encode("utf-8")
    req = Request(
        base_url.rstrip("/") + "/sql",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req, timeout=10) as resp:
        return json.loads(resp.read().decode("utf-8"))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Continuously write random kline rows through rsduck Web API.")
    parser.add_argument("--url", default="http://127.0.0.1:8080", help="rsduck Web base URL.")
    parser.add_argument("--interval", type=float, default=0.5, help="Write interval in seconds.")
    parser.add_argument("--batch", type=int, default=1, help="Rows written per interval.")
    parser.add_argument("--symbols", type=int, default=100, help="Number of random symbols to rotate through.")
    parser.add_argument("--print-sql-on-error", action="store_true", help="Print generated SQL when a write fails.")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    total = 0
    print(f"target: {args.url}")
    print(f"interval={args.interval}s batch={args.batch} symbols={args.symbols}")
    print("press Ctrl+C to stop")

    try:
        while True:
            rows = [build_row(args.symbols) for _ in range(args.batch)]
            sql = build_insert_sql(rows)
            try:
                data = post_sql(args.url, sql)
            except (HTTPError, URLError, TimeoutError) as exc:
                print(f"{datetime.now().strftime('%H:%M:%S')} write failed: {exc}")
                time.sleep(args.interval)
                continue

            if data.get("success"):
                total += len(rows)
                last = rows[-1]
                print(
                    f"{datetime.now().strftime('%H:%M:%S')} wrote={len(rows)} "
                    f"total={total} last={last[0]} close={last[5]}"
                )
            else:
                print(f"{datetime.now().strftime('%H:%M:%S')} write failed: {data.get('msg')}")
                if args.print_sql_on_error:
                    print(sql)

            time.sleep(args.interval)
    except KeyboardInterrupt:
        print(f"\nstopped, total rows written: {total}")


if __name__ == "__main__":
    main()

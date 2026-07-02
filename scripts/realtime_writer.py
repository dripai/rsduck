import argparse
import asyncio
import random
import signal
from datetime import datetime

import asyncpg


def build_row(symbol_count: int) -> tuple:
    code = f"6{random.randint(0, symbol_count - 1):05d}"
    base = random.uniform(4.0, 60.0)
    open_price = round(base, 2)
    high = round(base * random.uniform(1.000, 1.025), 2)
    low = round(base * random.uniform(0.975, 1.000), 2)
    close = round(random.uniform(low, high), 2)
    volume = random.randint(10_000, 800_000)
    return code, datetime.now(), open_price, high, low, close, volume


async def write_loop(args: argparse.Namespace) -> None:
    conn = await asyncpg.connect(args.dsn)
    stop_event = asyncio.Event()

    def request_stop() -> None:
        stop_event.set()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, request_stop)
        except NotImplementedError:
            pass

    sql = """
        INSERT OR REPLACE INTO kline_day
        (code, bar_time, open, high, low, close, volume)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
    """

    total = 0
    try:
        print(f"connected: {args.dsn}")
        print(f"interval={args.interval}s batch={args.batch} symbols={args.symbols}")
        while not stop_event.is_set():
            rows = [build_row(args.symbols) for _ in range(args.batch)]
            await conn.executemany(sql, rows)
            total += len(rows)
            last = rows[-1]
            print(
                f"{datetime.now().strftime('%H:%M:%S')} wrote={len(rows)} "
                f"total={total} last={last[0]} close={last[5]}"
            )
            try:
                await asyncio.wait_for(stop_event.wait(), timeout=args.interval)
            except asyncio.TimeoutError:
                pass
    finally:
        await conn.close()
        print(f"stopped, total rows written: {total}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Continuously write random kline rows to rsduck.")
    parser.add_argument(
        "--dsn",
        default="postgresql://127.0.0.1:15432/memory",
        help="PostgreSQL wire DSN for rsduck.",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=0.5,
        help="Write interval in seconds. Use 1 for one second, 0.5 for 500ms.",
    )
    parser.add_argument(
        "--batch",
        type=int,
        default=1,
        help="Rows written per interval.",
    )
    parser.add_argument(
        "--symbols",
        type=int,
        default=100,
        help="Number of random symbols to rotate through.",
    )
    return parser.parse_args()


if __name__ == "__main__":
    asyncio.run(write_loop(parse_args()))

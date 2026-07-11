from pathlib import Path


def main():
    try:
        import duckdb
    except ImportError as exc:
        raise SystemExit("install fixture dependency first: pip install duckdb") from exc

    output = Path(__file__).resolve().parents[1] / "fixtures" / "daily_quote.parquet"
    output.parent.mkdir(parents=True, exist_ok=True)
    connection = duckdb.connect()
    output_sql = str(output).replace("'", "''")
    connection.execute(
        "COPY (SELECT '688981.SH' AS code, DATE '2026-07-10' AS trade_date, 50.2 AS close UNION ALL SELECT '603986.SH', DATE '2026-07-10', 120.5) TO '%s' (FORMAT PARQUET)"
        % output_sql
    )
    connection.close()
    print("created %s" % output)


if __name__ == "__main__":
    main()

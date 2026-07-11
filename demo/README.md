# RSDuck Python demos

These scripts correspond to the programmatic scenarios in section 4 and the Web implementation verification in section 5 of `doc/rsduck-practical-examples.md`. They run against a separately started RSDuck Web service and use only the Web API.

## Run

```powershell
python demo/python/4_1_sector_full_sync.py
```

The default service URL is `http://127.0.0.1:13307`. Every script creates tables prefixed with its document section, such as `demo_4_1_sector_list`; it never uses the tables from the practical-examples document.

All Demo scripts use the fixed test credential `admin/admin`; passwords are intentionally not command-line parameters.

Shared options:

```text
--url       Web API base URL
--username  Login user, default: admin
--cleanup   Drop objects created by the script after verification
```

| Document section | Script | Scenario |
|---|---|---|
| 4.1 | `python/4_1_sector_full_sync.py` | Full sector and constituent synchronization |
| 4.2 | `python/4_2_sector_incremental_refresh.py` | Incremental refresh of one sector |
| 4.3 | `python/4_3_sector_daily_aggregation.py` | Daily sector aggregation |
| 4.4 | `python/4_4_parquet_import.py` | Web Parquet import |
| 4.5 | `python/4_5_data_quality_check.py` | Data quality checks |
| 4.6 | `python/4_6_snapshot_restore.py` | Snapshot save and manifest verification |
| 4.7 | `python/4_7_permission_audit.py` | Role, privilege, and audit workflow |
| 4.8 (HTTP) | `python/4_8_http_continuous_write.py` | Continuous HTTP writes with periodic throughput reporting |
| 4.8 (MySQL wire) | `python/4_8_mysql_continuous_write.py` | Continuous writes over one PyMySQL connection |
| 5.1 | `python/5_1_web_console_api_smoke.py` | Web API smoke test for SQL, metadata, and session |

## Parquet fixture

Section 15 needs a Parquet file below the service's configured `web.parquet_import_root`. Create one with the optional DuckDB Python package:

```powershell
pip install duckdb
python demo/python/4_4_make_parquet_fixture.py
python demo/python/4_4_parquet_import.py --source demo/fixtures/daily_quote.parquet
```

Section 17 calls the Web snapshot endpoint and validates the resulting snapshot directory. Recovery itself remains a service-startup operation, so run it only against an isolated demo service when testing restore.

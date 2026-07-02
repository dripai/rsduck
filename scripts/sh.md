python scripts\realtime_writer_http.py --interval 0.5

每秒写一次：
python scripts\realtime_writer_http.py --interval 1

每 500ms 批量写 10 条：
python scripts\realtime_writer_http.py --interval 0.5 --batch 10


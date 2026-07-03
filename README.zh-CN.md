# rsduck

语言: [English](README.md) | 简体中文

rsduck 是一个基于 DuckDB 的内存数据库中间件服务。它在进程内启动 DuckDB 内存库，对外提供 PostgreSQL wire 协议和 Web SQL 控制台，并通过目录快照实现内存库的持久化恢复。

## 功能概览

- 内存 DuckDB：启动后数据主要在内存中读写，适合低延迟分析查询。
- PostgreSQL wire 接入：外部工具可按 PG 协议连接 rsduck。
- Web SQL 控制台：浏览器中查看表列表、执行 SQL、分页查看结果、手工保存快照。
- 多读单写架构：读请求分发到 read workers，写请求进入 single write worker，降低读写互相阻塞。
- 目录快照：使用 DuckDB `EXPORT DATABASE` / `IMPORT DATABASE` 保存和恢复完整库。
- 初始化 SQL：没有快照时可通过 `init.sql` 初始化表结构。
- 压测脚本：`scripts/rsduck_load_test.py` 可持续写入并发查询，用于观察前端查询影响。
- GitHub Actions：远程 push 后会自动执行格式检查、测试和多平台 release 编译。

## 应用场景

- 高频写入、实时查询的临时分析库。
- 需要 PG 协议入口，但不想部署完整数据库服务的轻量场景。
- 股票 K 线、指标、日志、监控数据等内存分析查询。
- 本地研发、策略回测、数据实验、临时数据服务。
- 需要快速恢复的内存数据库服务，允许低频快照持久化。

## 架构设计

架构设计参考链接：[DuckDB 连接池与单写多读设计](doc/duckdb-pool-design.md)。

## 快速开始

### 1. 准备 `init.sql`

`init.sql` 是首次启动时的表结构初始化脚本。rsduck 只会在没有恢复快照时执行它。表结构和可选的初始化数据可以写在这里：

```sql
CREATE TABLE IF NOT EXISTS kline_day (
    code      VARCHAR NOT NULL,
    bar_time  TIMESTAMP NOT NULL,
    open      DOUBLE,
    high      DOUBLE,
    low       DOUBLE,
    close     DOUBLE,
    volume    BIGINT,
    PRIMARY KEY (code, bar_time)
);
```

仓库示例 `rsduck.toml` 已经把 `[db].init_sql` 指向这个文件：

```toml
[db]
init_sql = "init.sql"
```

如果没有 `rsduck.toml`，程序内置默认值是 `init_sql = ""`，也就是没有快照时启动一个空内存库。

### 2. 编译

开发编译：

```powershell
cargo build
```

正式编译：

```powershell
cargo build --release
```

构建产物位置取决于 Cargo 的 target 目录。如果设置了 `CARGO_TARGET_DIR`，产物会写入该目录；否则会写入当前仓库的 `target` 目录。当前环境的 `CARGO_TARGET_DIR` 指向 `D:\cargo-target`，所以通常在：

```text
D:\cargo-target\debug\rsduck.exe
D:\cargo-target\release\rsduck.exe
```

### 3. 启动服务

```powershell
D:\cargo-target\release\rsduck.exe
```

实际启动时请根据自己的构建产物存放路径调整可执行文件路径。

默认端口：

```text
PG wire: 127.0.0.1:15432
Web:     http://127.0.0.1:8080
```

## Web 控制台

Web 控制台左侧展示数据库表列表，右侧上方是 SQL 编辑区，下方是查询结果区。页面支持分页、手工保存快照，以及编辑区和结果区之间的拖动分割条。

![rsduck Web SQL 控制台](console.png)

## 程序连接查询和写入

rsduck 提供两个程序接入入口：

- HTTP SQL API：`http://127.0.0.1:8080/sql`
- PostgreSQL wire 协议：`127.0.0.1:15432`

### 使用 Python 标准库访问 HTTP API

这个例子不需要安装第三方 Python 包。它把完整 SQL 文本发给 Web API，可以查询，也可以写入：

```python
import json
from urllib.request import Request, urlopen

BASE_URL = "http://127.0.0.1:8080"

def run_sql(sql, page=0, page_size=1000):
    payload = json.dumps({
        "sql": sql,
        "page": page,
        "page_size": page_size,
    }).encode("utf-8")
    req = Request(
        BASE_URL + "/sql",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req, timeout=10) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    if not data["success"]:
        raise RuntimeError(data["msg"])
    return data

run_sql("""
INSERT INTO kline_day
(code, bar_time, open, high, low, close, volume)
VALUES
('600000', TIMESTAMP '2026-07-03 09:30:00', 10.1, 10.5, 9.9, 10.2, 120000)
""")

result = run_sql("SELECT code, close, volume FROM kline_day ORDER BY bar_time DESC LIMIT 10")
print(result["columns"])
print(result["rows"])
```

HTTP 请求格式：

```json
{
  "sql": "SELECT * FROM kline_day LIMIT 10",
  "page": 0,
  "page_size": 1000
}
```

HTTP 返回格式：

```json
{
  "columns": ["code", "close"],
  "rows": [["600000", "10.2"]],
  "success": true,
  "msg": "1 row(s)"
}
```

### 使用 PostgreSQL wire 协议访问

支持 PG 协议的工具和驱动可以连接 PG wire 端口。当前适配器不做账号鉴权，`database`、`user`、`password` 只是兼容字段，不代表 DuckDB 内部有多个数据库或用户。

连接参数：

```text
host:     127.0.0.1
port:     15432
database: postgres
user:     postgres
password: 任意值或留空
```

Python `psycopg` 示例：

```powershell
pip install "psycopg[binary]"
```

```python
import psycopg

conn = psycopg.connect(
    host="127.0.0.1",
    port=15432,
    dbname="postgres",
    user="postgres",
    password="postgres",
)

with conn:
    with conn.cursor() as cur:
        cur.execute("""
            INSERT INTO kline_day
            (code, bar_time, open, high, low, close, volume)
            VALUES
            ('600001', TIMESTAMP '2026-07-03 09:31:00', 11.0, 11.4, 10.8, 11.2, 90000)
        """)

        cur.execute("SELECT code, close, volume FROM kline_day ORDER BY bar_time DESC LIMIT 10")
        print(cur.fetchall())
```

持续写入和并发查询压测可以直接使用：

```powershell
python scripts\rsduck_load_test.py --write-interval 0.5 --write-batch 10 --query-workers 4 --query-interval 0.2
```

## 配置

默认配置文件为 `rsduck.toml`：

```toml
[db]
init_sql = "init.sql"
read_workers = 4
write_queue_size = 100000
read_queue_size = 1024
snapshot_queue_size = 16
max_result_rows = 100000

[snapshot]
restore_on_startup = true
dir = "snapshot"
prefix = "rsduck"
interval_secs = 900
retain_hours = 2

[pg]
bind = "127.0.0.1:15432"

[web]
enabled = true
bind = "127.0.0.1:8080"
```

启动恢复顺序：

1. 如果 `restore_on_startup = true`，扫描最新正式快照目录。
2. 如果找到快照，执行 `IMPORT DATABASE` 恢复完整库。
3. 如果没有快照，执行 `db.init_sql`。
4. 如果 `init_sql = ""`，启动空内存库。

参数说明，按 `rsduck.toml` 中的顺序排列：

- 【db.init_sql】初始化 SQL 文件路径。只在启动时没有恢复快照的情况下执行。用于创建表、索引、视图或初始化数据；设置为 `""` 表示启动空库。
- 【db.read_workers】专用 DuckDB 读 worker 线程数量。读 SQL 会分发到这些 worker。提高该值可以增强并发读能力，但也会增加 CPU 和内存压力。
- 【db.write_queue_size】写 SQL 的有界队列大小。写入会通过 single write worker 串行执行；队列满时，新写请求会快速失败，而不是无限等待。
- 【db.read_queue_size】每个读 worker 的有界队列大小。读队列满时，新读请求会快速失败。
- 【db.snapshot_queue_size】快照请求队列大小。定时快照、关闭服务快照、Web 手工快照都进入这个队列；队列满通常表示已经有快照在等待或执行。
- 【db.max_result_rows】单次 SQL 执行最多返回的行数上限，在 Web 分页包装之前生效。用于避免一次性返回过大的结果集。
- 【snapshot.restore_on_startup】启动时是否恢复最新正式快照。启用后，如果找到快照，就不会执行 `db.init_sql`。
- 【snapshot.dir】快照目录的根路径，用于读取和写入快照目录。
- 【snapshot.prefix】快照目录名前缀。正式快照目录格式为 `prefix_yyyyMMdd_HHmmss`，例如 `rsduck_20260703_120000`。
- 【snapshot.interval_secs】自动快照间隔，单位秒。服务运行期间，定时任务按这个间隔保存快照。
- 【snapshot.retain_hours】旧快照保留小时数。定时快照清理时会删除超过保留时间的正式快照目录。
- 【pg.bind】PostgreSQL wire 端口监听地址。保持 `127.0.0.1` 表示仅本机访问；只有需要外部客户端连接时才改成明确的局域网地址。
- 【web.enabled】是否启动 Web SQL 控制台。
- 【web.bind】Web 控制台和 HTTP SQL API 的监听地址。

## 快照

rsduck 使用目录快照保存完整 DuckDB 数据库：

```text
snapshot/
  rsduck_20260703_120000/
    schema.sql
    load.sql
    table_a.parquet
    table_b.parquet
```

保存时先写临时目录：

```text
snapshot/rsduck_yyyyMMdd_HHmmss.tmp
```

成功后重命名为正式目录：

```text
snapshot/rsduck_yyyyMMdd_HHmmss
```

Web 控制台右上角的 `Save Snapshot` 可以手工触发快照。

## 使用案例：K 线实时写入和查询

默认 `init.sql` 会创建 `kline_day` 表：

```sql
CREATE TABLE IF NOT EXISTS kline_day (
    code      VARCHAR NOT NULL,
    bar_time  TIMESTAMP NOT NULL,
    open      DOUBLE,
    high      DOUBLE,
    low       DOUBLE,
    close     DOUBLE,
    volume    BIGINT,
    PRIMARY KEY (code, bar_time)
);
```

启动 rsduck 后，打开 Web 控制台：

```text
http://127.0.0.1:8080
```

运行压测脚本，持续写入并并发查询：

```powershell
python scripts\rsduck_load_test.py --write-interval 0.5 --write-batch 10 --query-workers 4 --query-interval 0.2
```

在 Web 控制台执行查询：

```sql
SELECT * FROM kline_day ORDER BY bar_time DESC LIMIT 100;
```

也可以查看表信息：

```sql
SELECT schema_name, table_name, estimated_size, column_count
FROM duckdb_tables()
WHERE internal = false
ORDER BY schema_name, table_name;
```

## 自动构建和下载地址

GitHub Actions workflow 位于：

```text
.github/workflows/ci.yml
```

推送 `v*` tag 时会执行：

```text
cargo fmt --check
cargo test
cargo build --release
```

编译后的版本可以从这里下载：

- 最新 Release：[github.com/dripai/rsduck/releases/latest](https://github.com/dripai/rsduck/releases/latest)
- 全部 Release：[github.com/dripai/rsduck/releases](https://github.com/dripai/rsduck/releases)
- 每次 CI 运行的构建产物：[github.com/dripai/rsduck/actions/workflows/ci.yml](https://github.com/dripai/rsduck/actions/workflows/ci.yml)

workflow 会打包这些文件：

```text
rsduck-windows-x64.zip
rsduck-windows-service-setup-x64.exe
rsduck-linux-x64.tar.gz
rsduck-macos-arm64.tar.gz
rsduck-macos-x64.tar.gz
```

## 注册为服务

### Windows

从 Releases 下载 `rsduck-windows-service-setup-x64.exe`。这是最简单的 Windows 安装包：双击运行，选择安装目录，安装器会自动把 rsduck 注册为开机自启的 Windows 服务。

安装器会把 `rsduck.exe`、`rsduck.toml`、`init.sql`、WinSW 服务文件、`logs`、`snapshot` 放到你选择的安装目录下，并把这个目录作为服务工作目录。

如果只需要便携式控制台程序，不注册服务，使用 `rsduck-windows-x64.zip`。

服务管理命令：

```powershell
Get-Service rsduck
Start-Service rsduck
Stop-Service rsduck
```

卸载可以通过 Windows 应用/程序管理界面完成，也可以使用开始菜单里的 `Uninstall rsduck`。

### Linux

将发布包文件放到 `/opt/rsduck`：

```bash
sudo mkdir -p /opt/rsduck
sudo tar -xzf rsduck-linux-x64.tar.gz -C /opt/rsduck
sudo cp rsduck.toml init.sql /opt/rsduck/
```

创建 `/etc/systemd/system/rsduck.service`：

```ini
[Unit]
Description=rsduck in-memory DuckDB middleware service
After=network.target

[Service]
Type=simple
WorkingDirectory=/opt/rsduck
ExecStart=/opt/rsduck/rsduck
Restart=always
RestartSec=5
KillSignal=SIGINT
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

启用并启动服务：

```bash
sudo systemctl daemon-reload
sudo systemctl enable rsduck
sudo systemctl start rsduck
sudo systemctl status rsduck
```

### macOS

将发布包文件放到 `/usr/local/rsduck`：

```bash
sudo mkdir -p /usr/local/rsduck
sudo tar -xzf rsduck-macos-arm64.tar.gz -C /usr/local/rsduck
sudo cp rsduck.toml init.sql /usr/local/rsduck/
```

Intel 芯片的 macOS 使用 `rsduck-macos-x64.tar.gz`。

创建 `/Library/LaunchDaemons/com.rsduck.plist`：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.rsduck</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/rsduck/rsduck</string>
  </array>
  <key>WorkingDirectory</key>
  <string>/usr/local/rsduck</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/usr/local/rsduck/rsduck.out.log</string>
  <key>StandardErrorPath</key>
  <string>/usr/local/rsduck/rsduck.err.log</string>
</dict>
</plist>
```

加载并启动：

```bash
sudo chown root:wheel /Library/LaunchDaemons/com.rsduck.plist
sudo chmod 644 /Library/LaunchDaemons/com.rsduck.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.rsduck.plist
sudo launchctl enable system/com.rsduck
sudo launchctl kickstart -k system/com.rsduck
```

停止并卸载：

```bash
sudo launchctl bootout system /Library/LaunchDaemons/com.rsduck.plist
```

关于关闭前快照：rsduck 当前处理的是 Ctrl+C/SIGINT。上面的 Linux `systemd` 配置会发送 SIGINT。macOS 下如果必须立即持久化最新内存数据，建议在 `launchctl bootout` 前先手工保存一次快照。

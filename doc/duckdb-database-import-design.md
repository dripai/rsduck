# 普通 DuckDB 整库导入 RSDuck 设计与使用说明

> 状态：已实现，当前工作区已提供 `rsduck import-duckdb` 命令，待随版本发布。本文记录当前实现的功能边界、行为规则、操作方式和验证结果。

## 1. 目标

提供一个明确的本地迁移入口，将普通 DuckDB 文件中的全部用户 schema 下的普通持久表，批量迁移为同一个 RSDuck 目标 schema 中的受管内存表：

```text
本地 source.duckdb
  -> 只读发现全部用户 schema 下的普通持久表
  -> 将源 schema.table 映射为 target_schema.table
  -> 每张表临时导出为 Parquet
  -> 在 RSDuck 创建或复用目标 schema
  -> 每张表独立调用受管导入
  -> 校验行数并生成迁移报告
```

正式命令：

```text
rsduck import-duckdb
```

该命令解决的是普通 DuckDB 数据迁移，不把普通 `.duckdb` 文件解释成 RSDuck Snapshot。RSDuck Snapshot v3 的启动恢复规则保持不变。

## 2. 当前支持范围

当前实现支持：

- 读取与 CLI、RSDuck 服务位于同一台机器的本地 `.duckdb` 文件。
- 自动发现源数据库全部用户 schema 下的普通持久表，不要求用户指定源 schema。
- 将发现的表统一导入一个由 `--target-schema` 指定的目标 schema。
- 自动创建不存在的目标 schema，已存在时直接复用。
- 逐表导出、导入、校验和清理临时文件。
- 单表失败时不建立该表，并继续处理后续表。
- 目标表已存在时只让该表失败，不覆盖、不追加、不修改原表。
- 识别跨源 schema 的同名表冲突，两张冲突表都失败，其他表继续。
- 使用 `--tables` 选择部分表，用于小范围迁移或失败重试。
- 使用 `--dry-run` 执行只读预检并生成计划报告。
- 输出控制台摘要和 JSON 迁移报告。
- 源表总数不设上限；CLI 始终按单表请求顺序执行。

当前不支持：

- 从用户电脑上传 `.duckdb` 到远程 RSDuck 服务器。
- 将普通 DuckDB 文件直接 `ATTACH` 到运行中的 RSDuck。
- 为不同源 schema 分别映射不同目标 schema；当前所有源表都进入一个目标 schema。
- 自动迁移视图、macro、sequence、索引、约束、注释、用户、角色和权限。
- 自动迁移 DuckDB VSS/HNSW 物理索引。
- 自动覆盖、合并、追加或重命名已经存在的目标表。
- 通过 Parquet 静默降级 `FLOAT[N]` 等固定维度数组。
- 在进程被强制中断时保证写出最终报告。

## 3. 命令格式

PowerShell 示例：

```powershell
rsduck import-duckdb `
  --source "D:\data\source.duckdb" `
  --target-schema agent_crm `
  --endpoint http://127.0.0.1:13307 `
  --username admin `
  --password replace-with-password
```

最小参数：

```powershell
rsduck import-duckdb `
  --source source.duckdb `
  --target-schema agent_crm `
  --password replace-with-password
```

参数定义：

| 参数 | 必填 | 默认值 | 说明 |
|---|---|---|---|
| `--source` | 是 | 无 | 本地 DuckDB 文件的绝对路径或相对当前目录的路径 |
| `--target-schema` | 是 | 无 | RSDuck 中统一接收源表的目标 schema |
| `--password` | 是 | 无 | RSDuck 登录密码，直接使用明文参数；不读取环境变量 |
| `--endpoint` | 否 | `http://127.0.0.1:13307` | 正在运行的 RSDuck Web 地址 |
| `--username` | 否 | `admin` | 用于创建 schema 和导入表的 RSDuck 用户 |
| `--tables` | 否 | 全部普通表 | 逗号分隔的表白名单，支持 `table` 或 `schema.table` |
| `--dry-run` | 否 | `false` | 只执行源文件、表、类型和认证预检，不创建目标对象 |
| `--keep-temp` | 否 | `false` | 保留本次迁移生成的临时 Parquet |
| `--report` | 否 | 自动生成 | JSON 迁移报告输出路径 |
| `--if-exists` | 否 | `error` | 当前只允许 `error`，目标表存在时该表失败 |

以下参数明确不支持：

```text
--source-schema
--password-env
```

传入未知参数、重复参数、空参数或不支持的 `--if-exists` 值时，命令在迁移前失败并返回退出码 `2`。

## 4. 本地文件规则

“本地文件”指 `--source` 路径可由运行 CLI 的机器直接读取，并且 RSDuck 服务与 CLI 位于同一台机器、共享同一文件系统。

CLI 的当前行为：

1. 将源路径规范化为绝对路径。
2. 拒绝目录、不存在或无法打开的路径。
3. 使用 DuckDB `AccessMode::ReadOnly` 打开源文件。
4. 开启源数据库只读事务，使表发现、计数和导出基于同一次迁移读取过程。
5. 由 DuckDB 实际打开和验证文件，不依赖 `.duckdb` 后缀判断格式。
6. 自动发现当前源数据库中所有非内部、非临时的普通持久表。

如果 CLI 和 RSDuck 服务不在同一台机器，服务端返回的 Parquet 导入目录通常无法由 CLI 正确写入。当前版本不提供远程上传协议，也不会把服务器本地路径接口当作文件上传接口。

## 5. Schema 与命名空间映射

RSDuck 使用 DuckDB schema 作为场景命名空间，例如：

```sql
CREATE SCHEMA agent_crm;
CREATE SCHEMA agent_support;
CREATE SCHEMA agent_knowledge;
```

命令不要求也不允许用户指定源 schema。源 schema 由 CLI 从 DuckDB 元数据自动读取，目标 schema 由用户指定：

```text
source.duckdb:main.memory       -> RSDuck:agent_crm.memory
source.duckdb:sales.customer   -> RSDuck:agent_crm.customer
source.duckdb:support.ticket   -> RSDuck:agent_crm.ticket
```

规则如下：

- 目标 schema 不存在时，通过 RSDuck 受管 DDL 执行 `CREATE SCHEMA IF NOT EXISTS`。
- 目标 schema 已存在时直接复用。
- 目标 schema 和目标表名必须匹配 ASCII 标识符规则：首字符为字母或下划线，其余为字母、数字或下划线。
- 禁止使用 `rsduck_catalog`、`rsduck_internal`、`information_schema` 和 `pg_catalog` 作为目标 schema。
- 创建 schema 需要相应 Catalog 权限；导入表需要目标 schema 的 DDL 权限。
- 场景之间可以建立不同目标 schema；同一场景内的租户隔离仍建议使用 `tenant_id` 和权限规则。
- 目标 schema 创建成功后，即使全部表导入失败，目标 schema 仍然保留。

### 5.1 跨源 Schema 同名表

多个源 schema 可能包含相同表名。由于当前实现将所有表压平到一个目标 schema，以下映射会冲突：

```text
source.duckdb:crm.memory      -> RSDuck:agent_data.memory
source.duckdb:support.memory  -> RSDuck:agent_data.memory
```

当前规则是：

- 所有映射到同一目标表名的源表都标记失败。
- 不按发现顺序选择其中一张表。
- 不自动覆盖、合并或改名。
- 没有冲突的其他表继续导入。
- 可以使用限定名称重试，例如 `--tables crm.memory`，此时只选择该源表。

## 6. 迁移执行流程

### 6.1 预检

CLI 按以下顺序执行预检：

1. 校验命令参数、目标 schema 和源文件路径。
2. 以只读方式打开源 DuckDB 并开始读取事务。
3. 自动枚举全部用户 schema 下的普通持久表。
4. 应用 `--tables` 白名单，并确认指定表存在。
5. 标记压平后发生目标表名冲突的源表。
6. 检查固定维度数组等不能通过当前 Parquet 链路可靠往返的类型。
7. 登录 RSDuck，验证用户名和明文密码。
8. 输出迁移计划。

`--dry-run` 在此结束，写出状态为 `dry_run` 或 `dry_run_failed` 的报告，不读取 Parquet 导入目录，也不创建目标 schema 或表。

当前 dry-run 不查询目标表是否已存在。目标表冲突由正式导入时的受管 `/parquet-import` 请求判断。

以下错误会使整个命令在逐表处理前终止：

- 参数无效。
- 源文件不存在、不是文件或 DuckDB 无法只读打开。
- 源数据库没有普通持久表。
- `--tables` 指定的表不存在。
- RSDuck 认证失败。
- 无法读取服务端 Parquet 导入目录。
- 无法创建目标 schema 或本次任务的临时目录。

### 6.2 创建目标 Schema

非 dry-run 模式下，CLI 通过 RSDuck `/sql` 受管入口执行：

```sql
CREATE SCHEMA IF NOT EXISTS agent_crm;
```

CLI 不会独立打开 RSDuck 数据库，也不会绕过 `rsduck_catalog` 直接创建对象。

### 6.3 逐表迁移

每张表依次执行：

```text
读取源表行数
  -> 导出当前表到唯一临时 Parquet
  -> 调用一次单表 /parquet-import
  -> 比较源行数与目标返回行数
  -> 记录 succeeded、failed 或 unknown
  -> 清理当前表临时文件
  -> 继续下一张表
```

单表请求示例：

```json
POST /parquet-import

{
  "source": ".rsduck-import/job-20260715-001/000001.parquet",
  "schema": "agent_crm",
  "table": "memory"
}
```

CLI 不使用目录多表导入请求。每次只提交一张表，确保一张表失败不会回滚已成功的其他表。

### 6.4 行数不一致

如果 `/parquet-import` 返回成功，但目标行数与源表行数不一致，CLI 会通过受管 SQL 删除刚导入的目标表，并将该表标记失败。如果删除也失败，报告会同时记录行数不一致和清理失败原因。

### 6.5 网络结果未知

如果请求已经发出，但网络错误或响应无法解析，CLI 无法可靠判断服务端事务是否提交。该表标记为 `unknown`，不会擅自重试、删除或假设失败；其他表继续处理。

### 6.6 完成

全部表处理结束后：

1. 写入最终 JSON 报告。
2. 输出总表数、成功数、失败数、行数和错误原因。
3. 默认删除本次任务的临时 Parquet 和空任务目录。
4. `--keep-temp` 保留临时 Parquet，用于调试。
5. 只要存在 `failed` 或 `unknown` 表，进程返回退出码 `2`；已经成功的表继续保留。

## 7. 事务与失败规则

事务边界固定为“每张目标表一个事务”，不提供整个目标 schema 的跨表事务。

每张表内部由现有受管 Parquet 导入保证：

- 建立物理表、复制数据、写入 Catalog、journal、epoch 和 checksum 在同一受控事务内保持一致。
- 受管导入明确失败时，该表的物理对象和 Catalog 记录全部回滚。
- 不允许留下空表、半张表或只有 Catalog 没有物理表的状态。
- 已经成功提交的其他表不受影响。
- 失败后继续处理后续表。

典型行为：

```text
main.memory       成功  125300 rows
main.events       失败  unsupported type
crm.profile       成功  12410 rows
crm.settings      失败  relation already exists
```

最终保留 `memory` 和 `profile`，不会因为 `events` 或 `settings` 失败而删除成功表。

## 8. 表数量与执行模型

`import-duckdb` 不限制源数据库的总表数。即使源数据库包含 2,000 张表，也会按顺序执行 2,000 次单表迁移。

现有 `/parquet-import` 目录批量请求最多 256 张表的保护限制保持不变。CLI 每次只提交一张表，因此不受该限制影响。

执行规则：

- 顺序导入，不启动无上限并发。
- RSDuck 写操作进入现有串行写 Worker。
- 默认一次只保留当前表的临时 Parquet，避免大库一次性占满磁盘。
- 控制台输出当前表序号、总表数、源表、目标表、耗时和行数。
- 用户强制中断时，已提交表继续保留；当前请求可能处于未知状态，且当前版本不保证写出最终报告。

## 9. 对象和类型迁移边界

| 源对象 | 当前行为 |
|---|---|
| 全部用户 schema 下的普通持久表 | 自动发现并迁移 |
| 表数据 | 自动迁移并校验行数 |
| 源 schema 名 | 写入报告，但不作为目标 schema |
| 表名、列名 | 在目标能力允许范围内保留 |
| Parquet 可可靠往返且 RSDuck 支持的列类型 | 保留 |
| 主键、唯一、外键、检查约束 | 不自动迁移 |
| 普通索引 | 不自动迁移 |
| 视图、macro、sequence | 不自动迁移 |
| 注释 | 不自动迁移 |
| 用户、角色、权限 | 不自动迁移 |
| DuckDB VSS/HNSW 索引 | 不迁移物理索引 |
| 临时表、系统表 | 忽略 |

Parquet 不能可靠保留 `FLOAT[N]` 固定维度语义。CLI 不会把固定 ARRAY 静默降级为可变长 `FLOAT[]`。遇到固定维度向量表时，该表在预检中失败，其他表继续。

如果后续需要迁移固定维度向量表，应先通过受管 DDL 按源类型创建目标表，再显式装载数据，并通过 Vector API 重建 HNSW；不能直接沿用当前通用 Parquet 路径。

## 10. 目标表冲突规则

当前 `--if-exists` 只支持 `error`：

- 目标表不存在：正常导入。
- 目标表已存在：该表标记失败，不修改目标表，继续后续表。
- 不自动执行 `DROP TABLE`。
- 不追加数据。
- 不比较后合并。
- 不自动改名。

如果需要重新迁移，管理员应先确认目标数据可以删除，再通过受管 DDL 删除对应表，或者指定新的目标 schema。

## 11. 临时目录与安全

CLI 登录 RSDuck 后，从 `GET /parquet-import` 获取规范化后的服务端导入根目录，并在其下创建唯一任务目录：

```text
{parquet_import_root}/.rsduck-import/{migration_id}/
```

安全规则：

- 源文件以只读模式打开。
- 临时文件只写入 RSDuck 配置的 `parquet_import_root` 下。
- 目标 schema、表名和文件路径经过校验或转义，不直接拼接不受控 SQL。
- `--password` 按要求直接接收明文，不读取环境变量。
- 密码、Session Cookie 和认证令牌不会写入迁移报告或正常控制台日志。
- 使用者需要自行注意：明文命令行参数可能被 Shell 历史或操作系统进程查看工具记录。
- 默认清理临时文件；只有显式 `--keep-temp` 才保留。
- RSDuck 服务账户必须拥有临时目录读取权限，CLI 用户必须拥有创建和删除权限。

## 12. 迁移报告

控制台摘要示例：

```text
DuckDB import plan
Source:        D:\data\source.duckdb
Target schema: agent_crm
Tables:        4

[1/4] main.memory -> agent_crm.memory
  OK: 125300 row(s)
[2/4] main.events -> agent_crm.events
  FAILED: unsupported DuckDB type for rsduck catalog: BLOB

DuckDB import summary
Status:    partial_failure
Total:     4
Succeeded: 3
Failed:    1
```

JSON 报告字段与当前实现一致：

```json
{
  "migration_id": "job-20260715-100000-1234-1",
  "source": "D:/data/source.duckdb",
  "target_schema": "agent_crm",
  "started_at": "2026-07-15T10:00:00+08:00",
  "finished_at": "2026-07-15T10:02:30+08:00",
  "status": "partial_failure",
  "total": 2,
  "succeeded": 1,
  "failed": 1,
  "tables": [
    {
      "source_schema": "main",
      "source_table": "memory",
      "target_table": "memory",
      "status": "succeeded",
      "source_rows": 125300,
      "target_rows": 125300,
      "elapsed_ms": 9320,
      "error": null
    }
  ]
}
```

未指定 `--report` 时，报告默认写入当前目录：

```text
rsduck-import-{migration_id}.json
```

可以使用报告中的限定源表名重试失败表：

```powershell
rsduck import-duckdb `
  --source source.duckdb `
  --target-schema agent_crm `
  --password replace-with-password `
  --tables main.events,crm.settings
```

## 13. 退出状态

当前实现的退出码：

| 退出码 | 含义 |
|---:|---|
| `0` | 正式导入全部成功，或 dry-run 没有预检失败表 |
| `1` | 运行期整体错误，例如源文件、认证、服务连接、目标 schema 或报告写入失败 |
| `2` | 参数错误、存在失败或状态未知的表、dry-run 发现预检失败表 |

脚本和 CI 应同时检查退出码和 JSON 报告，不应只解析控制台文本。

## 14. 实现约束

- CLI 是运行中 RSDuck 服务的客户端，不能绕过服务锁直接打开 RSDuck 内存实例。
- 目标 schema、表和 Catalog 必须通过现有受管 mutation 创建。
- 迁移不得通过 `ATTACH source.duckdb` 后直接 `CREATE TABLE AS SELECT` 绕过 Catalog。
- 现有 `/parquet-import` 的多表原子语义和 256 张保护限制保持不变。
- CLI 使用单表请求获得部分成功语义，不修改普通 Web 批量导入契约。
- 缺少源数据、类型映射、权限或临时目录时明确报错，不切换到其他导入来源。
- 迁移功能不改变 RSDuck Snapshot v3 格式和恢复流程。

## 15. 已验证能力

当前自动化测试已验证：

```text
[x] 参数解析要求 --source、--target-schema 和明文 --password
[x] 明确拒绝 --source-schema、--password-env 和未知参数
[x] 自动发现全部用户 schema 下的普通持久表
[x] --tables 支持 table 和 schema.table
[x] 跨源 schema 同名表冲突时只失败冲突表
[x] 固定维度 FLOAT[N] 在 Parquet 导出前明确失败
[x] 源 DuckDB 以只读模式打开
[x] 目标 schema 通过受管 SQL 自动创建
[x] 每次只导入一张表
[x] 单表失败无物理表或 Catalog 残留
[x] 一张表失败不影响前后成功表
[x] 目标表存在时只失败该表且不覆盖原数据
[x] 行数校验和 JSON 报告准确
[x] 临时文件默认清理，--keep-temp 可保留
[x] dry-run 不创建目标对象
```

使用项目 `data/stock.duckdb` 的真实数据验证结果：

```text
源表数量：10
成功：10
失败：0
总行数：15,802,313
最大单表：15,033,241 行
```

完整 Rust 测试结果为 130 个测试通过，另有 2 个需要下载 DuckDB VSS 扩展的既有测试按条件忽略。

仍需在 Release 环境继续验证：

```text
[ ] Linux 本地文件路径和权限
[ ] macOS 本地文件路径和权限
[ ] Windows、Linux、macOS Release 包中的命令可用性
[ ] 强制中断后的状态确认与报告恢复能力
```

# Agent 向量记忆检索与索引接入规范

## 1. 职责边界

- MySQL 等关系型业务数据库是记忆正文、业务状态和版本的事实源。
- RSDuck 只保存可重建的向量记录、HNSW 索引定义和运行状态；检索只返回 `memory_id + distance`，Agent 再回事实源批量读取正文。
- 正式路径固定为 `FLOAT[N] + Catalog 管理的 HNSW + Vector API`。精确模式只在调用方显式提交 `mode=exact` 时运行，不会在 ANN 失败时隐式扫描全表。
- 一个 `vector_space` 只能对应一个 embedding 模型、模型版本、固定维度和距离算法。模型或维度变更时必须创建新空间、重新生成向量并切换流量，不能原地混用。

推荐召回顺序：Agent 请求 RSDuck → 获得有序 `memory_id` → 业务库按 ID 批量读取且再次校验租户/Agent → 按 RSDuck 返回顺序重排 → 过滤已删除或不可见记忆 → 交给重排或提示词组装。

## 2. 启用 VSS

运行时配置：

```toml
[db]
extension_dir = "extensions"
vss_enabled = true

[web.vector_api_limits]
max_body_bytes = 33554432
max_concurrent_requests = 64
search_timeout_ms = 5000
write_timeout_ms = 30000
maintenance_timeout_ms = 300000
```

离线服务包会预置与当前 DuckDB 平台匹配的 VSS 文件。手工准备命令：

```text
rsduck prepare-vss-extension --dir extensions
```

当前 Rust DuckDB 依赖固定为 `1.10504.0`，运行时 DuckDB 为 `v1.5.4`。扩展只允许加载 `vss`；VSS 缺失或加载失败时服务明确报错，不回退到其他扩展或精确检索。

## 3. 向量表和索引

向量表必须由受管 DDL 创建，首版只支持普通非分区表。必需字段和业务唯一键如下：

```sql
CREATE TABLE agent_memory_vector (
    tenant_id BIGINT NOT NULL,
    agent_id BIGINT NOT NULL,
    memory_id BIGINT NOT NULL,
    source_version BIGINT NOT NULL,
    content_hash VARCHAR NOT NULL,
    embedding FLOAT[1536] NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    UNIQUE (tenant_id, agent_id, memory_id)
);
```

浏览器登录会话用于创建和维护索引。创建索引：

```http
POST /api/vector/indexes
Content-Type: application/json

{
  "vector_space": "agent-memory-text-embedding-3-small-v1",
  "schema": "main",
  "table": "agent_memory_vector",
  "column": "embedding",
  "index_name": "agent_memory_vector_hnsw",
  "embedding_model": "text-embedding-3-small",
  "model_version": "1",
  "metric": "cosine",
  "m": 16,
  "m0": 32,
  "ef_construction": 128,
  "default_ef_search": 64
}
```

`metric` 只允许 `cosine`、`l2sq`、`ip`。状态、重建和压缩接口：

```text
GET  /api/vector/indexes/{vector_space}/status
POST /api/vector/indexes/{vector_space}/rebuild
POST /api/vector/indexes/{vector_space}/compact
```

状态响应包含定义版本、物理代次、VSS 版本、维度、算法、参数、向量数量、状态和最近错误。创建依次经过 `pending → building → active`；重建和压缩使用 `rebuilding`、`compacting`，失败进入 `failed`；VSS 版本变化进入 `stale`；物理索引缺失或启动发现中断操作进入 `unavailable`。ANN 只接受 `active`，其他状态返回对应的稳定错误。

HNSW 是派生数据。快照保存向量数据、Catalog 和索引定义；恢复时先按 Catalog 恢复 `FLOAT[N]` 表，再重新创建物理 HNSW。Snapshot format 3 和 Catalog schema 2 不隐式解释旧格式。

## 4. 机器认证与权限

Agent 服务使用 Bearer Token，不使用浏览器 Cookie。Token 至少 32 个字符，配置中必须明确租户、可选 Agent、向量空间和操作权限：

```toml
[[web.vector_api_tokens]]
token = "replace-with-at-least-32-random-characters"
username = "agent_service"
tenant_ids = [1001]
agent_ids = [2001, 2002]
vector_spaces = ["agent-memory-text-embedding-3-small-v1"]
permissions = ["search", "write"]
```

- `agent_ids = []` 表示该 Token 可访问所列租户下的所有 Agent。
- `permissions` 只允许 `search` 和 `write`。
- 索引创建、状态、重建和压缩只接受浏览器登录会话，并继续受 RSDuck 用户权限控制。
- RSDuck 只在内存中保存 Token 的 SHA-256 摘要；真实 Token 不得提交到 Git、日志或错误信息。

## 5. 写入和删除

批量写入，每批 1～1000 条：

```http
POST /api/vector/upsert-batch
Authorization: Bearer <token>
Content-Type: application/json

{
  "vector_space": "agent-memory-text-embedding-3-small-v1",
  "items": [{
    "tenant_id": 1001,
    "agent_id": 2001,
    "memory_id": 90001,
    "source_version": 7,
    "content_hash": "sha256:...",
    "embedding": [0.12, 0.35, 0.78]
  }]
}
```

批量删除使用相同业务键和事件版本：

```http
POST /api/vector/delete-batch
Authorization: Bearer <token>
Content-Type: application/json

{
  "vector_space": "agent-memory-text-embedding-3-small-v1",
  "items": [{
    "tenant_id": 1001,
    "agent_id": 2001,
    "memory_id": 90001,
    "source_version": 8
  }]
}
```

同一批次在一个事务内完成。规则如下：

- `source_version` 必须单调递增且非负。
- 已存版本更高时返回 `STALE_SOURCE_VERSION`，整批回滚。
- 写入的版本与哈希都相同时计入 `idempotent`；版本相同但哈希不同返回 `SOURCE_VERSION_CONFLICT`。
- 删除不存在的记录计入 `idempotent`；旧版本删除不得删除新记录。
- 同一批次不得重复提交相同 `(tenant_id, agent_id, memory_id)`。
- 向量长度必须等于空间维度，拒绝 NaN、Infinity；cosine 额外拒绝全零向量。

Outbox 消费者只有在收到成功响应后才能确认消息。网络超时且结果未知时应使用完全相同的 `source_version + content_hash` 重试；不能为重试生成新版本。

## 6. 检索

```http
POST /api/vector/search
Authorization: Bearer <token>
Content-Type: application/json

{
  "vector_space": "agent-memory-text-embedding-3-small-v1",
  "tenant_id": 1001,
  "agent_id": 2001,
  "embedding": [0.12, 0.35, 0.78],
  "top_k": 20,
  "mode": "ann",
  "ef_search": 64
}
```

`top_k` 范围为 1～1000，`mode` 只允许 `ann` 或 `exact`，`ef_search` 必须大于 0。服务在同一读 Worker 连接上设置、执行并恢复 `hnsw_ef_search`，不会把会话参数泄漏给后续请求。相同距离按 `memory_id` 升序稳定排序。

成功响应示例：

```json
{
  "success": true,
  "error_code": null,
  "trace_id": "06d2e63e62264518a69b2f611dbdd4fd",
  "msg": "ok",
  "result": {
    "vector_space": "agent-memory-text-embedding-3-small-v1",
    "mode": "ann",
    "index_status": "active",
    "matches": [{"memory_id": 90001, "distance": 0.031}]
  }
}
```

## 7. 错误与重试

所有 Vector API 业务响应都包含 `success`、`error_code`、`trace_id` 和 `msg`。主要稳定错误码：

| 错误码 | HTTP | 是否建议重试 |
|---|---:|---|
| `AUTHENTICATION_FAILED` | 401 | 否，修复 Token |
| `AUTHORIZATION_FAILED` / `TENANT_SCOPE_DENIED` | 403 | 否，修复权限或范围 |
| `VECTOR_SPACE_NOT_FOUND` | 404 | 否，修复空间配置 |
| `VECTOR_DIMENSION_MISMATCH` / `INVALID_VECTOR_VALUE` | 400 | 否，修复请求 |
| `INVALID_BATCH_SIZE` / `INVALID_TOP_K` / `INVALID_SEARCH_MODE` | 400 | 否，修复请求 |
| `DUPLICATE_VECTOR_KEY` / `SOURCE_VERSION_CONFLICT` | 409 | 否，修复事件 |
| `STALE_SOURCE_VERSION` | 409 | 否，丢弃旧事件 |
| `INDEX_BUILDING` / `INDEX_STALE` / `INDEX_UNAVAILABLE` / `VSS_UNAVAILABLE` | 503 | 是，查询状态并退避；不得在服务端隐式精确扫描 |
| `RATE_LIMITED` | 429 | 是，指数退避 |
| `REQUEST_BODY_TOO_LARGE` | 413 | 否，缩小或拆分批次 |
| `REQUEST_TIMEOUT` | 504 | 结果未知；写入和删除使用原版本幂等重试，维护操作先查询状态 |
| `SERVICE_UNAVAILABLE` / `VECTOR_OPERATION_FAILED` | 503/500 | 仅在请求幂等时退避重试 |

默认 HTTP 请求体上限为 32 MiB、并发请求上限为 64，搜索/写入/维护默认超时分别为 5 秒、30 秒和 5 分钟。批量上限仍固定为 1000；高维向量应按实际 JSON 大小拆批。请求超时不会谎报数据库操作已取消：后台操作继续完成，并发许可在真实完成前不会释放。因此超时后的写删必须使用原 `source_version + content_hash` 幂等重试，重建或压缩则先查询状态。

## 8. 模型升级

1. 创建新向量表或至少创建新的 `vector_space`，名称中包含模型与版本。
2. 从事实源全量读取有效记忆，用新模型生成固定维度向量并批量写入。
3. 比较新旧空间的 Recall、延迟和内存，确认新索引为 `active`。
4. Agent 配置切换到新空间；不得把两个模型的向量写入同一空间。
5. 保留观察期后再通过受管 DDL 删除旧索引和旧表。

## 9. 基准测试

工具直接生成确定性向量，以精确查询为真值，输出 JSON：

```text
cargo run --release --bin vector-benchmark -- \
  --rows 100000 --dimension 384 --queries 100 --top-k 10,100 \
  --mutation-rows 1000 --m 16 --m0 32 \
  --ef-construction 128 --ef-search 64 \
  --extension-dir extensions --output benchmark-100k-384.json
```

报告包含 Recall@K、精确/ANN p50/p95/p99、数据生成、索引构建、批量写入/删除、压缩、重建耗时，以及 RSDuck 基准进程峰值 RSS。正式容量验收应覆盖 384/768/1024/1536 维、10 万/100 万/500 万行、不同 HNSW 参数和 2/4/8/16/32 GB 资源档位；这些大规模结果必须在目标机器实际执行后归档，不能用小规模结果外推替代。

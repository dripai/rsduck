# Agent Vector Memory Retrieval and Indexing Contract

Language: English | [中文](agent-vector-memory.md)

## 1. Responsibility Boundaries

- A relational business database such as MySQL is the source of truth for memory text, business state, and versions.
- RSDuck stores only rebuildable vector records, HNSW index definitions, and runtime state. Retrieval returns only `memory_id + distance`; the Agent reads text from the source of truth in batches.
- The production path is fixed as `FLOAT[N] + Catalog-managed HNSW + Vector API`. Exact mode runs only when the caller explicitly submits `mode=exact`; an ANN failure never triggers an implicit full-table scan.
- One `vector_space` can use only one embedding model, model version, fixed dimension, and distance metric. A model or dimension change requires a new space, regenerated vectors, and an explicit traffic switch. Do not mix them in place.

Recommended retrieval sequence: the Agent requests RSDuck → receives ordered `memory_id` values → the business database reads them in batches and revalidates tenant/Agent → results are reordered by RSDuck order → deleted or invisible memories are filtered → candidates are passed to reranking or prompt assembly.

## 2. Enable VSS

Runtime configuration:

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

Offline service packages include the VSS file matching the current DuckDB platform. To prepare it manually:

```text
rsduck prepare-vss-extension --dir extensions
```

The Rust DuckDB dependency is fixed at `1.10504.0`, with DuckDB runtime `v1.5.4`. Only the `vss` extension may be loaded. A missing or failed VSS load is an explicit service error; RSDuck does not fall back to another extension or exact retrieval.

## 3. Vector Tables and Indexes

Vector tables must be created through managed DDL. The first version supports ordinary non-partitioned tables only. Required fields and the business uniqueness key are:

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

A browser login session creates and maintains indexes. Create an index with:

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

`metric` permits only `cosine`, `l2sq`, and `ip`. Status, rebuild, and compaction endpoints are:

```text
GET  /api/vector/indexes/{vector_space}/status
POST /api/vector/indexes/{vector_space}/rebuild
POST /api/vector/indexes/{vector_space}/compact
```

The status response includes definition version, physical generation, VSS version, dimension, metric, parameters, vector count, state, and the latest error. Creation progresses through `pending → building → active`; rebuild and compaction use `rebuilding` and `compacting`; failures enter `failed`; a VSS version change enters `stale`; and a missing physical index or interrupted operation discovered at startup enters `unavailable`. ANN accepts only `active`; every other state returns its stable error.

HNSW is derived data. Snapshots save vector data, the Catalog, and index definitions. Restore first recreates the `FLOAT[N]` table from the Catalog and then recreates physical HNSW indexes. Snapshot format 3 and Catalog schema 2 do not implicitly interpret older formats.

## 4. Machine Authentication and Authorization

Agent services use a Bearer Token, not a browser Cookie. Tokens contain at least 32 characters. Configuration must state tenants, optional Agents, vector spaces, and permitted operations:

```toml
[[web.vector_api_tokens]]
token = "replace-with-at-least-32-random-characters"
username = "agent_service"
tenant_ids = [1001]
agent_ids = [2001, 2002]
vector_spaces = ["agent-memory-text-embedding-3-small-v1"]
permissions = ["search", "write"]
```

- `agent_ids = []` means the token can access every Agent in the listed tenants.
- `permissions` permits only `search` and `write`.
- Index creation, status, rebuild, and compaction accept only a browser login session and remain subject to RSDuck user authorization.
- RSDuck keeps only the SHA-256 token digest in memory. Never commit an actual token to Git or expose it in logs or error messages.

## 5. Writes and Deletes

Batch upserts accept 1 to 1,000 records per batch:

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

Batch deletion uses the same business key and event version:

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

Every batch completes in one transaction. The rules are:

- `source_version` must be non-negative and monotonically increasing.
- If the stored version is higher, the service returns `STALE_SOURCE_VERSION` and rolls back the entire batch.
- An upsert with the same version and hash is `idempotent`; the same version with a different hash returns `SOURCE_VERSION_CONFLICT`.
- Deleting a nonexistent record is `idempotent`; an old-version delete cannot remove a newer record.
- A batch cannot contain the same `(tenant_id, agent_id, memory_id)` more than once.
- Vector length must equal the space dimension. NaN and Infinity are rejected; cosine additionally rejects all-zero vectors.

An Outbox consumer acknowledges a message only after receiving a success response. If a network timeout leaves the result unknown, retry with the exact same `source_version + content_hash`; never generate a new version for a retry.

## 6. Retrieval

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

`top_k` ranges from 1 to 1,000. `mode` permits only `ann` and `exact`, and `ef_search` must be greater than zero. On the same read-worker connection, RSDuck sets, executes with, and restores `hnsw_ef_search`, so no session parameter leaks to a later request. Equal distances are stably ordered by ascending `memory_id`.

Example successful response:

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

## 7. Errors and Retries

Every Vector API business response includes `success`, `error_code`, `trace_id`, and `msg`. The main stable error codes are:

| Error code | HTTP | Retry guidance |
|---|---:|---|
| `AUTHENTICATION_FAILED` | 401 | No; fix the token. |
| `AUTHORIZATION_FAILED` / `TENANT_SCOPE_DENIED` | 403 | No; fix permissions or scope. |
| `VECTOR_SPACE_NOT_FOUND` | 404 | No; fix vector-space configuration. |
| `VECTOR_DIMENSION_MISMATCH` / `INVALID_VECTOR_VALUE` | 400 | No; fix the request. |
| `INVALID_BATCH_SIZE` / `INVALID_TOP_K` / `INVALID_SEARCH_MODE` | 400 | No; fix the request. |
| `DUPLICATE_VECTOR_KEY` / `SOURCE_VERSION_CONFLICT` | 409 | No; fix the event. |
| `STALE_SOURCE_VERSION` | 409 | No; discard the obsolete event. |
| `INDEX_BUILDING` / `INDEX_STALE` / `INDEX_UNAVAILABLE` / `VSS_UNAVAILABLE` | 503 | Yes; query status and back off. Never perform an implicit exact scan on the server. |
| `RATE_LIMITED` | 429 | Yes; use exponential backoff. |
| `REQUEST_BODY_TOO_LARGE` | 413 | No; split or reduce the batch. |
| `REQUEST_TIMEOUT` | 504 | Result unknown; retry writes and deletes idempotently with the original version. Query status before maintenance actions. |
| `SERVICE_UNAVAILABLE` / `VECTOR_OPERATION_FAILED` | 503/500 | Back off only when the request is idempotent. |

Default HTTP body limit is 32 MiB, concurrent-request limit is 64, and default search/write/maintenance timeouts are 5 seconds, 30 seconds, and 5 minutes. The batch limit remains 1,000. Split high-dimensional vectors according to actual JSON size. A request timeout does not claim the database action was canceled: the background action continues, and its concurrency permit is not released until actual completion. Therefore, timed-out writes and deletes must retry idempotently with the original `source_version + content_hash`; rebuild and compaction must query status first.

## 8. Model Upgrades

1. Create a new vector table or at least a new `vector_space` whose name includes the model and version.
2. Read all valid memories from the source of truth, generate fixed-dimension vectors with the new model, and write them in batches.
3. Compare recall, latency, and memory of the old and new spaces, and verify the new index is `active`.
4. Switch the Agent configuration to the new space. Do not write two models' vectors to one space.
5. After an observation period, delete the old index and old table through managed DDL.

## 9. Benchmarking

The tool generates deterministic vectors, uses exact queries as the ground truth, and emits JSON:

```text
cargo run --release --bin vector-benchmark -- \
  --rows 100000 --dimension 384 --queries 100 --top-k 10,100 \
  --mutation-rows 1000 --m 16 --m0 32 \
  --ef-construction 128 --ef-search 64 \
  --extension-dir extensions --output benchmark-100k-384.json
```

The report includes Recall@K; exact and ANN p50/p95/p99; data generation, index build, batch upsert/delete, compaction, and rebuild times; and the RSDuck benchmark process peak RSS. Formal capacity acceptance must cover 384/768/1024/1536 dimensions, 100 thousand/1 million/5 million rows, varied HNSW parameters, and 2/4/8/16/32 GB resource tiers. Archive those large-scale results after running them on the target machines; never substitute an extrapolation from a small run.

# RSDuck: Turning Agent Memory from Stored Data into Retrieved Context

Language: English | [中文](rsduck-vector-memory-overview.md)

The difficult part of a large-language-model Agent is often not a single conversation. It is finding the small set of historically valuable memories from a much larger collection. Built on DuckDB, RSDuck provides fixed-dimension vectors, managed HNSW indexes, and a dedicated Vector API for this semantic-retrieval stage.

It does not replace a business database such as MySQL. Instead, it accelerates memory retrieval: the business database holds text, state, permissions, and versions; RSDuck finds the relevant `memory_id` values by semantic similarity; and the business service reads the source of truth by ID. This division prevents two systems from becoming competing sources of truth while allowing an Agent to retrieve long-term memory at scale.

## 1. Why Agents Need Vector Memory

Traditional database queries are strong at matching explicit conditions: user, time, label, or keyword. User requests, however, are often semantically related without sharing the same words.

For example, a historical memory may say, “The customer wants the monthly operating report changed to a weekly report, sent on Monday morning.” A user may later ask, “When are we supposed to send that report?” The words differ, but the meaning is the same.

Vector memory addresses this **semantic similarity despite different wording**:

1. An embedding model converts text into an array of floating-point values, called a vector.
2. Text with similar meaning generally produces nearby vectors in a high-dimensional space.
3. The current question's vector retrieves semantically related historical vectors.
4. RSDuck returns `memory_id + distance`; the Agent then reads the original text, permissions, and current business state from the business database.

An Agent can therefore use durable long-term memory instead of relying only on the current chat window.

## 2. Where RSDuck Fits in the Architecture

```text
Domain Agent / business service
        |
        | writes business facts, state, and text
        v
MySQL or another relational business database  <---+
        |                                           |
        | Outbox event                              | reads text in batches by memory_id
        v                                           |
Embedding Worker -- fixed-dimension vectors --> RSDuck
                                                 |
                               HNSW semantic search | returns memory_id + distance
                                                 v
                                         Agent assembles context
```

Two boundaries are essential:

- **MySQL is the source of truth.** Memory text, deletion state, visibility, tenant relationships, and business versions remain in the business database.
- **RSDuck is a rebuildable retrieval layer.** It stores vectors, index definitions, and runtime state. Rebuilding an index must never lose business facts.

RSDuck therefore returns candidate memories, not text that bypasses business authorization. The business service must validate tenant, Agent, and visibility again when reading the text.

## 3. From Text to Similarity: How Vector Retrieval Works

### 3.1 Embeddings: Mapping Meaning into Coordinates

An embedding model converts text into a fixed-length numeric array, for example 1,536 dimensions:

```text
"Send the customer a weekly report on Monday"
  -> [0.12, 0.35, -0.08, ..., 0.78]
```

An individual value normally has no business-readable meaning. Together, the values locate the text in semantic space. When the same model processes “When is the weekly report due?” and “Send the operating report on Monday morning,” the resulting vectors tend to be close.

RSDuck expresses this contract with `FLOAT[N]`, where `N` is a fixed dimension such as `FLOAT[1536]`. Dimension is not an arbitrary column detail: it is part of the vector space. A vector space must use one embedding model, model version, dimension, and distance metric. A model upgrade requires a new space, regenerated vectors, and an explicit traffic switch.

### 3.2 Distance: Defining “Close”

Retrieval computes the distance between a query vector and historical vectors. Cosine distance, for example, compares vector direction and is commonly suitable for text embeddings.

```sql
SELECT
    memory_id,
    list_cosine_distance(
        embedding,
        [0.12, 0.35, 0.78]::FLOAT[]
    ) AS distance
FROM agent_memory_vector
WHERE tenant_id = 1001
  AND agent_id = 2001
ORDER BY distance
LIMIT 20;
```

This is an **exact search**. It calculates distance row by row inside the tenant and Agent scope, so it is exact but its cost grows linearly with the number of vectors. It is appropriate for debugging, acceptance checks, and benchmarks.

Production retrieval normally uses an ANN (Approximate Nearest Neighbor) index. ANN accepts a small chance of missing marginal candidates in exchange for much lower latency and more predictable resource consumption.

## 4. HNSW: Why It Is Fast

HNSW stands for Hierarchical Navigable Small World. A useful mental model is a layered graph for navigating among nearby vectors.

Instead of scanning every vector, it connects similar vectors in advance:

```text
Upper layer: a few long-range navigation nodes    A -------- B -------- C
                                                     \                 /
Lower layer: many local-neighbor nodes            A1--A2--A3 ... B1--B2 ... C1--C2
```

A search roughly proceeds as follows:

1. Start at a small number of nodes in an upper layer to find the relevant region quickly.
2. Descend layer by layer into denser local-neighbor graphs.
3. Expand enough candidates in the lowest layer and return the `top_k` memories with the smallest distances.

It is like locating the right district on a city map first, then following streets to an address, rather than asking every household in the country.

Common HNSW parameters are:

- `m` / `m0`: the number of neighbors retained per node. Higher values usually improve recall but consume more memory and take longer to build.
- `ef_construction`: the candidate-search width while building the index. Higher values generally improve graph quality but increase build time.
- `ef_search`: the candidate-search width at query time. Higher values generally improve recall but increase query latency.

There is no universally correct parameter set. Use actual dimensions, actual data volume, and the business `top_k` to compare recall, p95 latency, and RSS memory usage.

## 5. RSDuck Compared with Direct DuckDB Vector Use

RSDuck is not a separate vector engine. Its current vector capability is still based on **DuckDB + the VSS extension + HNSW**. RSDuck turns those low-level components into a managed service for production Agent workloads.

| Dimension | Direct DuckDB vector use | RSDuck vector retrieval |
|---|---|---|
| Access model | Local SQL or embedded-program calls | HTTP Vector API and managed SQL |
| Vector field | Defined and validated by the application | Fixed-dimension `FLOAT[N]` validation |
| Similarity query | Application writes SQL such as `list_cosine_distance` | `search` API returns `memory_id + distance` |
| HNSW index | Application loads VSS, creates indexes, and tunes parameters | Catalog records definitions, parameters, state, and physical generation |
| Index failures | Application detects, alerts, and recovers | Explicit `building`, `active`, `stale`, `failed`, and `unavailable` states |
| Multi-tenant boundary | Application adds `WHERE` conditions | Token, tenant, Agent, and vector-space restrictions together |
| Duplicate events | Application implements event idempotency | `source_version + content_hash` govern upsert and delete events |
| Snapshot and restore | Application restores tables, indexes, and version relationships | Stores vectors and index definitions, then rebuilds HNSW from the Catalog |
| Flexibility | High: direct access to native DuckDB capabilities | Constrained by Catalog and API contracts for consistent production behavior |

The preceding SQL is a typical direct-DuckDB **exact-search** path: it computes distance for every row in the filtered scope. The result is exact, but the computational cost grows roughly linearly with vector count. It is useful for debugging, acceptance testing, and a recall baseline.

RSDuck production traffic normally calls `POST /api/vector/search` with `mode=ann` and uses HNSW for approximate nearest-neighbor retrieval. It trades a small possibility of missing marginal candidates for faster, more stable latency at large memory scale. The two paths complement each other: use RSDuck ANN for online retrieval and exact SQL for offline quality comparison and troubleshooting.

The choice is straightforward: direct DuckDB is lighter for local analysis, prototypes, small data volumes, or fully flexible SQL. RSDuck is better for multiple Agents and tenants, Outbox synchronization, long-running services, and systems that need authorization, index state, rebuilds, and recovery.

## 6. RSDuck's Strengths: More Than “Creating a Vector Index”

### 6.1 Fixed-Dimension Vectors Are a Verifiable Data Contract

`FLOAT[N]` validates dimension at write time and rejects NaN and Infinity. For cosine distance, it also rejects zero vectors. This prevents vectors from different models or dimensions from being silently mixed into one column and corrupting retrieval quality at the data layer.

### 6.2 HNSW Is Catalog-Managed, Not a One-Off SQL Side Effect

RSDuck records vector space, model version, distance metric, HNSW parameters, physical index generation, and runtime state in `rsduck_catalog.rs_vector_index`. Creation, rebuild, compaction, and recovery have explicit states:

```text
pending -> building -> active
                 |         |
                 v         v
              failed     rebuilding / compacting

VSS version change: stale
Missing physical index or interrupted recovery: unavailable
```

Only an `active` index can serve ANN. While an index is being built, VSS is unavailable, or the index is invalid, the service returns a stable error. It does not silently fall back to an exact full-table scan and hide a capacity issue as an occasional slow request.

### 6.3 Controlled VSS Extension Loading

RSDuck manages DuckDB's VSS extension and restricts loading to the required `vss` extension. Missing extensions, version mismatches, and load failures are explicit errors. Distribution packages include the extension file for the target platform, avoiding the uncertainty of discovering an unavailable index only after deployment.

### 6.4 A Dedicated Vector API for Agent Event Streams

The Vector API supports batch upserts, deletes, search, index status, rebuild, and compaction. Writes use `(tenant_id, agent_id, memory_id)` as the business key and use a monotonic `source_version` for at-least-once Outbox delivery:

- Duplicate events can succeed idempotently.
- Old events cannot overwrite newer data.
- The same version with different content is an explicit conflict.
- A batch completes in one transaction; failure leaves no partial batch behind.

Tokens are also restricted by tenant, Agent, vector space, and `search/write` permissions, preventing an Agent from retrieving another tenant's memory.

### 6.5 Indexes Are Derived Data with a Clear Recovery Path

Snapshot v3 stores vector data, the Catalog, and index definitions. Restore first recreates `FLOAT[N]` tables and then rebuilds physical HNSW indexes from the Catalog. A snapshot therefore does not depend on a non-portable index binary and can clearly determine whether an index must be rebuilt after a platform or VSS version change.

## 7. Suitable Use Cases

| Scenario | What RSDuck does | What the business database still owns |
|---|---|---|
| Long-term domain-Agent memory | Retrieves semantically similar historical memories for the current question | Original text, permissions, business state, and deletion markers |
| Customer-service or sales assistant | Finds similar tickets, opportunities, and customer preferences | Customer master data, ticket workflow, and compliance rules |
| Knowledge-base Q&A | Retrieves IDs of relevant document chunks | Document text, versions, publication state, and ACLs |
| Engineering assistant | Finds related incidents, decisions, and code explanations | Issues, code repositories, permissions, and current status |
| Operations-analysis assistant | Finds similar campaigns, retrospectives, and strategy notes | Metric facts, approvals, and final reports |

It is especially suitable for systems that retrieve a small candidate set first, then apply business rules and generate an answer. It is not a replacement for strict-condition queries, accounting-consistency queries, or primary business paths that require an absolutely exact Top-K. Use the relational source of truth or explicit `exact` search for those cases.

## 8. Example: Customer-Memory Retrieval for a Domain Agent

Assume a domain Agent follows up with enterprise customers. The business database contains these memories:

```text
memory_id=90001: The customer asked to change the monthly operating report to a weekly report sent every Monday morning.
memory_id=90002: The customer is price-sensitive; prepare a package comparison before renewal.
memory_id=90003: The customer confirmed the data-interface allowlist last week.
```

### Write Path

1. The business service writes the memory text to MySQL and emits an Outbox event with a version.
2. The Embedding Worker reads the event and calls a model to generate a `FLOAT[1536]` vector.
3. The worker calls RSDuck `upsert-batch` with the business key, `source_version`, content hash, and vector.
4. After RSDuck validates tenant, Agent, dimension, and version, it writes the vector table; HNSW serves later ANN retrieval.

### Query Path

The user asks: “When do we send that customer's report?”

1. The Agent generates a query vector with the same embedding model.
2. It calls `POST /api/vector/search` with fixed `tenant_id`, `agent_id`, `top_k`, and `mode=ann`.
3. RSDuck returns candidates such as `90001 (distance=0.031)` and `90003 (distance=0.274)`.
4. The Agent reads those IDs from MySQL in a batch, verifies they remain visible and not deleted, then restores RSDuck's distance order.
5. The Agent uses the text of `90001` to answer: “The recorded agreement is to send it every Monday morning.”

If `90001` is later changed or deleted in the business system, a newer Outbox event updates or deletes RSDuck's vector record with a higher `source_version`. Even if an old event arrives late, it cannot overwrite newer data.

## 9. The Five Most Important Integration Rules

1. Use one model, model version, dimension, and distance metric in a single `vector_space`.
2. Keep business text and authorization in the source of truth; RSDuck returns only candidate IDs and distances.
3. Every write and delete must carry a monotonic `source_version`; retries after timeout use the original version and content hash.
4. Use `mode=ann` for production by default. When an index is not `active`, the caller must retry or degrade according to an explicit policy; RSDuck does not implicitly scan the full table.
5. Create a new vector space for a model upgrade. Backfill it, evaluate it, and switch traffic; never mix vectors from different models.

## 10. Conclusion

RSDuck's value is not merely that DuckDB “supports vectors.” It brings the fixed-dimension contract, index lifecycle, VSS dependency, tenant isolation, event idempotency, snapshot recovery, and performance benchmarking required for Agent memory retrieval into one managed capability.

For an Agent system, this creates a clear separation between business facts and semantic retrieval: the business database keeps data true and controllable, while RSDuck makes long-term memory fast to retrieve as it grows.

For table definitions, API details, authentication configuration, error codes, and model-upgrade procedures, see the [Agent Vector Memory Retrieval and Indexing Contract](agent-vector-memory.md).

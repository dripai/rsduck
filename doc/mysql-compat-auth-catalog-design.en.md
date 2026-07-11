# rsduck MySQL Compatibility, Authentication, and Catalog Design

Language: English | [中文](mysql-compat-auth-catalog-design.md)

This document describes how rsduck provides a MySQL-compatible experience on top of DuckDB, and how authentication, authorization, and rsduck's own catalog are managed. The goal is not to fully clone MySQL. The goal is to define which capabilities are projected, which capabilities are rejected, and why `rsduck_catalog.rs_*` must remain the single source of truth.

## 1. Design Positioning

rsduck exposes a MySQL wire protocol endpoint mainly to:

- Let Navicat and common MySQL clients connect.
- Support common queries, prepared statements, `SHOW ...`, and metadata probing.
- Support basic user, role, and privilege management.
- Make tables, views, functions, and privileges visible to clients from rsduck's current state.

rsduck does not attempt to implement full MySQL semantics:

- It does not implement MySQL storage engines.
- It does not implement MySQL events, triggers, or a complete system database.
- It does not treat `mysql.*` tables as a real catalog.
- It does not make `information_schema` a second source of truth.

Core principles:

```text
MySQL protocol compatibility is an adapter.
rsduck_catalog.rs_* is the source of truth.
DuckDB is the physical execution engine.
```

## 2. MySQL Protocol Layer

The MySQL service is implemented under `src/server/mysql` and listens on `[mysql].bind`.

Connection flow:

```text
TCP accept
  -> send handshake
  -> receive auth response
  -> authenticate against rsduck catalog
  -> create MySqlSession
  -> command loop
```

The command loop mainly handles:

```text
COM_QUERY
COM_STMT_PREPARE
COM_STMT_EXECUTE
COM_STMT_CLOSE
COM_STMT_RESET
COM_PING
COM_INIT_DB
COM_QUIT
```

Unsupported commands return a MySQL error packet. They are not ignored and are not reported as successful.

## 3. Session Model

A MySQL session stores:

- username
- current database
- prepared statement map
- next statement id

`COM_INIT_DB` only changes the session database. rsduck does not have MySQL's multi-database storage model. Actual schema resolution follows these rules:

- Empty database or `memory` maps to `main`.
- Other database names are treated as schema names.

This lets Navicat operate with MySQL-style database behavior while rsduck still uses DuckDB schemas and the rsduck catalog internally.

## 4. Authentication Design

Both Web and MySQL authentication go through `DbHandle::authenticate` and the same catalog-backed authentication logic. Authentication requests include:

- protocol type
- username
- plaintext password or MySQL challenge response

Users are stored in:

```text
rsduck_catalog.rs_user
```

Important fields include:

- `username`
- `password_hash`
- `password_algo`
- `mysql_auth_plugin`
- `mysql_auth_string`
- `status`
- `last_login_at`

rsduck stores both the password hash used by the service itself and the verifier needed by the MySQL protocol. Creating a user or changing a password must update both credential forms.

```sql
CREATE USER quant_reader PASSWORD='replace_me';
ALTER USER quant_reader PASSWORD 'new_password';
```

The default administrator `admin/admin` is only for initialization. Production or long-running development environments should change it immediately after startup.

## 5. Authorization Model

Authorization is centered on `SessionPrincipal`, which contains:

- user id
- username
- roles

Admin detection is direct: a user with the `admin` role is treated as an administrator.

Normal permission checks are grouped into three scopes:

```text
system   -> system action
schema   -> schema action
relation -> relation action
```

Privilege records are stored in:

```text
rsduck_catalog.rs_privilege
```

Privilege targets are represented by:

```text
principal_type  user / role
principal_id
object_type     system / schema / relation
object_id
action
```

Checks consider:

- Direct user privileges.
- Privileges inherited from roles.
- Admin role short-circuit.
- Relation read inherited from schema read.

Permission denial writes an audit log:

```text
target: rsduck_audit
event: permission_denied
```

## 6. Privilege Action Mapping

rsduck does not copy every MySQL privilege bit. It maps external privileges to internal actions:

```text
relation SELECT/READ/USAGE -> read
relation INSERT/UPDATE/DELETE -> write
relation CREATE/DROP/OWNERSHIP -> ddl

schema SELECT/READ/USAGE -> read
schema CREATE/DROP/OWNERSHIP -> ddl

system -> manage_snapshot / manage_catalog / manage_user
```

Example:

```sql
CREATE ROLE analyst;
GRANT SELECT ON TABLE market.daily_quote TO ROLE analyst;
GRANT ROLE analyst TO quant_reader;
```

Or grant directly to a user:

```sql
GRANT SELECT ON TABLE market.daily_quote TO quant_reader;
GRANT INSERT ON TABLE market.daily_quote TO quant_reader;
GRANT CREATE ON SCHEMA market TO quant_reader;
```

Do not write `rs_privilege` directly. Use `GRANT/REVOKE` so journal, checksum, audit, and compatibility projections stay consistent.

## 7. SQL Authorization Entry Point

External SQL enters execution through:

```text
route_sql
  -> worker
  -> execute_typed_sql_blocking / describe_sql_blocking
  -> authorize_sql
  -> execute or reject
```

Authorization extracts objects and actions from the statement type:

- Querying a table requires relation `read`.
- Writing a table requires relation `write`.
- Creating schemas, tables, views, or indexes requires schema or system management privileges.
- User, role, and privilege management requires system `manage_user` or admin.
- Snapshot operations require system `manage_snapshot` or admin.

DDL catalog mutation enters explicit execution functions such as create table, grant, and drop. Authorization is not a string blacklist. It is based on objects, actions, and catalog records.

## 8. Catalog as Source of Truth

Managed metadata is stored in `rsduck_catalog`:

```text
rs_catalog_version   catalog version, epoch, checksum
rs_oid_alloc         OID allocator
rs_catalog_journal   catalog mutation journal
rs_schema            schema
rs_type              type
rs_relation          tables, views, indexes, and other relations
rs_column            columns
rs_column_default    defaults
rs_constraint        primary key, unique, foreign key, check constraints
rs_index             indexes
rs_dependency        object dependencies
rs_comment           comments
rs_relation_ext      rsduck extension properties
rs_partition         partition status
rs_user              users
rs_role              roles
rs_user_role         user-role mapping
rs_privilege         privileges
```

Reserved schemas:

```text
rsduck_catalog
rsduck_internal
pg_catalog
information_schema
```

External SQL cannot modify these reserved areas. `information_schema` and `mysql.*` are compatibility projections, not sources of truth.

## 9. Catalog-Aware Mutation

DDL cannot be blindly passed through to DuckDB. `execute_catalog_aware_write_as` dispatches supported DDL into explicit mutations:

```text
CREATE SCHEMA       -> create_schema
CREATE USER/ROLE    -> create_user_account / create_role_account
ALTER USER          -> alter_user_account
CREATE TABLE        -> create_table_relation
CREATE VIEW         -> create_view_relation
CREATE INDEX        -> create_index_relation
ALTER TABLE         -> alter_table_relation
DROP                -> drop_objects
COMMENT ON          -> comment_object
GRANT/REVOKE        -> grant_privileges / revoke_privileges
managed partition   -> create_range_partitioned_table
```

Every catalog-aware mutation must maintain:

1. Permission check.
2. Pending journal record.
3. DuckDB physical object mutation.
4. `rs_*` catalog records.
5. Dependencies.
6. Completed journal record.
7. Epoch and checksum.
8. Rollback on failure.

The goal is to keep client-visible objects, privilege decisions, snapshot restore, and DuckDB physical state consistent.

### ALTER TABLE Policy

Column rename and type changes cannot update only `rsduck_catalog`: physical data and catalog records must remain consistent in the same transaction.

- Ordinary-table rename: execute DuckDB DDL, then update the matching `rs_column` name. The attribute number, comments, and attribute-number-based dependencies remain stable.
- Ordinary-table type change: execute DuckDB DDL so it converts existing data, then update `rs_column.atttypid` and related metadata from the resulting physical column definition.
- Non-partition column of a partitioned table: execute the same DDL on every active physical partition, then synchronize parent and child catalog records and refresh the logical entrypoint view. Any partition failure rolls back the entire transaction.
- Partition-key rename: in addition to the preceding steps, update `rs_relation_ext.partition_key` and refresh future write routing. Reject the operation when external dependent views exist, avoiding views that retain the old column name.
- Partition-key type change: not supported, because it affects partition boundaries, routing-value formats, and `partition_unit` semantics.

Type changes do not use a separate pre-scan. DuckDB DDL is the single actual conversion validation. If indexes, constraints, or other DuckDB dependencies block the change, rsduck returns DuckDB's dependency or conversion error.

Before a column rename or type change, rsduck checks for external dependent views. If any exist, the operation is rejected so that no view definition is left invalid after the structural change.

## 10. Why rsduck Does Not Create MySQL System Tables

Navicat queries many MySQL system tables, for example:

```text
mysql.user
mysql.db
mysql.role_edges
mysql.default_roles
mysql.tables_priv
mysql.columns_priv
mysql.procs_priv
```

rsduck does not create these as real DuckDB tables because:

- They would become a second metadata source competing with `rsduck_catalog`.
- MySQL field semantics do not exactly match rsduck's privilege model.
- Writing to those tables would not naturally trigger journal, checksum, snapshot, or permission checks.
- Long-term maintenance would cost more than controlled projections.

The correct approach is to rewrite Navicat queries to read-only projections:

```text
mysql.user       -> projection over rs_user and rs_user_role
mysql.db         -> projection over rs_privilege and rs_schema
mysql.role_edges -> projection over rs_user_role
```

These projections are read-only. Their fields are shaped for MySQL client expectations, while the true state remains in the rsduck catalog.

## 11. information_schema Projections

Currently supported `information_schema` relations include:

```text
schemata
tables
views
routines
parameters
columns
statistics
table_constraints
key_column_usage
```

Projection sources are combined from:

- `rsduck_catalog.rs_*`
- DuckDB metadata table functions
- Current user's privileges
- rsduck extension fields such as managed kind and availability status

For example, a table list must consider:

- Whether the DuckDB physical table exists.
- Whether `rs_relation` has a managed record.
- Whether the relation is available.
- Whether the current user has read permission.
- Whether the schema is reserved.

This avoids a common failure mode: Navicat shows an object, but double-clicking it fails because DuckDB or the catalog cannot find the relation.

## 12. SHOW Compatibility

MySQL clients commonly use:

```sql
SHOW TABLES;
SHOW COLUMNS FROM t;
SHOW INDEX FROM t;
SHOW ENGINES;
SHOW VARIABLES;
SELECT DATABASE();
SELECT VERSION();
```

rsduck handles these in two ways:

- Constant compatibility results such as `SHOW ENGINES`, `SHOW VARIABLES`, and `SELECT VERSION()`.
- Rewrites to controlled catalog or `information_schema` queries, such as `SHOW TABLES`, `SHOW COLUMNS`, and `SHOW INDEX`.

This lets Navicat load the object tree without letting clients bypass the catalog and read DuckDB internal system tables directly.

## 13. SQL Dialect Compatibility

The MySQL protocol layer performs limited rewrites before sending SQL to DuckDB:

```text
`identifier`       -> "identifier"
LIMIT offset,count -> LIMIT count OFFSET offset
? placeholders     -> $1, $2, ...
```

These rewrites only cover compatibility points that are clearly required. rsduck does not maintain a full MySQL parser and does not perform broad SQL semantic conversion. SQL that cannot be explicitly supported should return an error.

## 14. Prepared Statements

Prepared statement flow:

```text
COM_STMT_PREPARE
  -> parse original SQL
  -> rewrite ? to $n
  -> apply MySQL compatibility rewrite
  -> describe_sql_with_params_as
  -> allocate statement id
  -> return parameter and column metadata

COM_STMT_EXECUTE
  -> parse binary parameters
  -> execute_typed_sql_with_params_as
  -> return binary result rows or OK packet
```

Parameters are converted to rsduck internal `SqlParam` values before entering DuckDB. Returned column types are mapped to MySQL column definitions.

## 15. Type Display

rsduck internal results use neutral types:

```text
SqlType
SqlValue
SqlTypedResult
```

The Web API returns `sql_type` and `mysql_type`. The MySQL protocol layer maps `SqlType` to MySQL column type, charset, flags, and decimals.

This means:

- Physical execution remains DuckDB.
- Web display is not coupled to MySQL packet details.
- MySQL clients see compatible types.

When adding a new type, update all of:

- DuckDB to rsduck catalog type mapping.
- Web `sql_type/mysql_type` display.
- MySQL column type mapping.
- Snapshot save and restore.
- Parquet import.
- Tests.

## 16. Snapshot and Catalog Relationship

Snapshot v2 exports catalog metadata separately as:

```text
catalog.duckdb
```

Business data is exported per relation:

```text
data/<rel_oid>.parquet
```

Reasons:

- The catalog is the metadata source of truth and should be saved as a whole.
- Business table data can be split per relation for row-count checks and partial unavailable marking.
- MySQL compatibility projections do not need persistence because they can be regenerated from the catalog and DuckDB metadata.

Therefore `mysql.*` and `information_schema.*` should not enter the snapshot as physical tables. Snapshots contain the rsduck catalog and managed business objects.

## 17. Navicat Compatibility Strategy

Navicat's object tree triggers many metadata queries. rsduck's strategy is:

1. Support only queries that have been observed and confirmed as necessary.
2. Use controlled projections for object tree, table, column, index, view, function, and privilege queries.
3. Return clear errors for unsupported relations instead of creating empty system-table shells.
4. Add protocol tests or at least keep query samples whenever a new Navicat compatibility point is added.

This is more restrained than creating every MySQL system table, and it better preserves rsduck's source-of-truth principle.

## 18. Security Boundaries

These boundaries must remain:

- Web API must require login and must not become an unauthenticated management interface.
- MySQL authentication must use `rs_user`; anonymous connections are not accepted.
- External SQL cannot write `rsduck_catalog`, `rsduck_internal`, `information_schema`, or `pg_catalog`.
- Users, roles, and privileges can only be changed through DDL or management commands.
- Manual snapshot save requires `manage_snapshot`.
- Parquet import paths must stay under `web.parquet_import_root`.
- Unsupported MySQL metadata relations should not be automatically passed through to DuckDB internal catalog tables.

## 19. Development Rules for New Compatibility Features

When adding a MySQL/Navicat compatibility point, evaluate in this order:

1. Is this query required for client startup, object tree loading, editing, or privilege management?
2. Can a read-only projection be built from `rsduck_catalog.rs_*` and DuckDB official metadata functions?
3. Would this introduce a second source of truth?
4. Does it need privilege filtering?
5. Does it affect Snapshot v2?
6. Is it covered by protocol tests?

Recommended implementation:

```text
mysql_compat::rewrite_sql
  -> relation-specific projection SQL
  -> existing DbHandle execution path
```

Avoid:

```text
CREATE TABLE mysql.user (...)
INSERT fake rows
let client query it directly
```

## 20. Summary

rsduck's MySQL compatibility layer is an adapter:

- Protocol behavior tries to satisfy MySQL clients.
- Metadata always returns to `rsduck_catalog.rs_*`.
- Execution always returns to DuckDB.
- Privileges always go through rsduck principal/privilege checks.
- Persistence always enters Snapshot v2.

With this boundary clear, Navicat compatibility, Web management, Snapshot restore, and the internal catalog can keep one consistent semantic model.

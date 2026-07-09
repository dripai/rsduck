use super::*;

pub(super) fn pg_namespace_sql() -> String {
    format!(
        "
    SELECT
        CAST(n.oid AS VARCHAR) AS oid,
        n.nspname,
        CAST(n.nspowner AS VARCHAR) AS nspowner,
        '' AS nspacl,
        COALESCE(d.description, '') AS description,
        n.nspname AS schema_name,
        COALESCE(u.username, 'unknown') AS schema_owner
    FROM rsduck_catalog.pg_namespace n
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = n.nspowner
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = n.oid AND d.classoid = {PG_NAMESPACE_CLASSOID} AND d.objsubid = 0
    WHERE n.nspname NOT IN ('rsduck_catalog', 'rsduck_internal')
    ORDER BY
        CASE WHEN n.nspname = 'main' THEN 0
             WHEN n.nspname IN ('pg_catalog', 'information_schema') THEN 2
             ELSE 1
        END,
        n.nspname
    "
    )
}

pub(super) fn information_schema_schemata_sql() -> String {
    format!(
        "
    SELECT
        current_database() AS catalog_name,
        n.nspname AS schema_name,
        COALESCE(u.username, 'unknown') AS schema_owner,
        current_database() AS default_character_set_catalog,
        'pg_catalog' AS default_character_set_schema,
        'UTF8' AS default_character_set_name,
        '' AS sql_path,
        COALESCE(d.description, '') AS description
    FROM rsduck_catalog.pg_namespace n
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = n.nspowner
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = n.oid AND d.classoid = {PG_NAMESPACE_CLASSOID} AND d.objsubid = 0
    WHERE n.nspname NOT IN ('rsduck_catalog', 'rsduck_internal')
    ORDER BY
        CASE WHEN n.nspname = 'main' THEN 0
             WHEN n.nspname IN ('pg_catalog', 'information_schema') THEN 2
             ELSE 1
        END,
        n.nspname
    "
    )
}

pub(super) fn pg_class_sql() -> String {
    "
    SELECT
        CAST(c.oid AS VARCHAR) AS oid,
        c.relname AS relname,
        CAST(c.relnamespace AS VARCHAR) AS relnamespace,
        CAST(c.reltype AS VARCHAR) AS reltype,
        '0' AS reloftype,
        CAST(c.relowner AS VARCHAR) AS relowner,
        '0' AS relam,
        '0' AS relfilenode,
        '0' AS reltablespace,
        '0' AS relpages,
        CAST(c.reltuples AS VARCHAR) AS reltuples,
        '0' AS relallvisible,
        '0' AS reltoastrelid,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS relhasindex,
        'f' AS relisshared,
        c.relpersistence AS relpersistence,
        c.relkind AS relkind,
        CAST(c.relnatts AS VARCHAR) AS relnatts,
        CAST((SELECT COUNT(*) FROM rsduck_catalog.pg_constraint con WHERE con.conrelid = c.oid AND con.contype = 'c') AS VARCHAR) AS relchecks,
        'f' AS relhasrules,
        'f' AS relhastriggers,
        'f' AS relhassubclass,
        'f' AS relrowsecurity,
        'f' AS relforcerowsecurity,
        't' AS relispopulated,
        'd' AS relreplident,
        CASE WHEN c.relispartition THEN 't' ELSE 'f' END AS relispartition,
        '0' AS relrewrite,
        '0' AS relfrozenxid,
        '0' AS relminmxid,
        '' AS relacl,
        c.reloptions AS reloptions,
        c.relpartbound AS relpartbound,
        n.nspname AS nspname,
        n.nspname AS schemaname,
        c.relname AS tablename,
        c.relname AS table_name,
        COALESCE(u.username, 'unknown') AS tableowner,
        '' AS tablespace,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS hasindexes,
        'f' AS hasrules,
        'f' AS hastriggers,
        'f' AS rowsecurity,
        COALESCE(d.description, '') AS description,
        c.status AS rsduck_status,
        c.error_message AS rsduck_error_message
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = c.oid AND d.objsubid = 0
    WHERE c.status IN ('active', 'unavailable')
      AND COALESCE(ext.visibility, 'user') = 'user'
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

pub(super) fn pg_tables_sql() -> String {
    "
    SELECT
        n.nspname AS schemaname,
        c.relname AS tablename,
        c.relname AS table_name,
        COALESCE(u.username, 'unknown') AS tableowner,
        '' AS tablespace,
        CASE WHEN c.relhasindex THEN 't' ELSE 'f' END AS hasindexes,
        'f' AS hasrules,
        'f' AS hastriggers,
        'f' AS rowsecurity,
        c.status AS rsduck_status,
        c.error_message AS rsduck_error_message
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    WHERE c.status IN ('active', 'unavailable')
      AND COALESCE(ext.visibility, 'user') = 'user'
      AND c.relkind IN ('r', 'p')
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

pub(super) fn information_schema_tables_sql() -> String {
    "
    SELECT
        current_database() AS table_catalog,
        n.nspname AS table_schema,
        c.relname AS table_name,
        CASE WHEN c.relkind = 'v' THEN 'VIEW' ELSE 'BASE TABLE' END AS table_type,
        '' AS self_referencing_column_name,
        '' AS reference_generation,
        '' AS user_defined_type_catalog,
        '' AS user_defined_type_schema,
        '' AS user_defined_type_name,
        CASE WHEN c.relkind IN ('r', 'p') THEN 'YES' ELSE 'NO' END AS is_insertable_into,
        'NO' AS is_typed,
        '' AS commit_action,
        c.status AS rsduck_status,
        c.error_message AS rsduck_error_message
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    WHERE c.status IN ('active', 'unavailable')
      AND COALESCE(ext.visibility, 'user') = 'user'
      AND c.relkind IN ('r', 'p', 'v')
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

pub(super) fn pg_attribute_sql(include_dropped: bool) -> String {
    let dropped_filter = if include_dropped {
        ""
    } else {
        "      AND a.attisdropped = FALSE\n"
    };
    format!(
        "
    SELECT
        CAST(a.attrelid * 10000 + a.attnum AS VARCHAR) AS oid,
        CAST(a.attrelid AS VARCHAR) AS attrelid,
        a.attname AS attname,
        CAST(a.atttypid AS VARCHAR) AS atttypid,
        '-1' AS attstattarget,
        '-1' AS attlen,
        CAST(a.attnum AS VARCHAR) AS attnum,
        '0' AS attndims,
        '-1' AS attcacheoff,
        CAST(a.atttypmod AS VARCHAR) AS atttypmod,
        'f' AS attbyval,
        'x' AS attstorage,
        'i' AS attalign,
        CASE WHEN a.attnotnull THEN 't' ELSE 'f' END AS attnotnull,
        CASE WHEN a.atthasdef THEN 't' ELSE 'f' END AS atthasdef,
        'f' AS atthasmissing,
        a.attidentity AS attidentity,
        a.attgenerated AS attgenerated,
        CASE WHEN a.attisdropped THEN 't' ELSE 'f' END AS attisdropped,
        't' AS attislocal,
        '0' AS attinhcount,
        '0' AS attcollation,
        '' AS attacl,
        a.attoptions AS attoptions,
        '' AS attfdwoptions,
        '' AS attmissingval,
        n.nspname AS table_schema,
        c.relname AS table_name,
        a.attname AS column_name,
        CAST(a.attnum AS VARCHAR) AS ordinal_position,
        t.rsduck_physical_type AS data_type,
        CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable,
        COALESCE(def.adbin, '') AS column_default,
        COALESCE(d.description, '') AS description
    FROM rsduck_catalog.pg_attribute a
    JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.pg_type t ON t.oid = a.atttypid
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.pg_attrdef def
      ON def.adrelid = a.attrelid AND def.adnum = a.attnum
    LEFT JOIN rsduck_catalog.pg_description d
      ON d.objoid = a.attrelid AND d.objsubid = a.attnum
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
{dropped_filter}
    ORDER BY n.nspname, c.relname, a.attnum
    "
    )
}

pub(super) fn information_schema_columns_sql() -> String {
    "
    SELECT
        current_database() AS table_catalog,
        n.nspname AS table_schema,
        c.relname AS table_name,
        a.attname AS column_name,
        CAST(a.attnum AS VARCHAR) AS ordinal_position,
        COALESCE(def.adbin, '') AS column_default,
        CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable,
        CASE t.typname
            WHEN 'bool' THEN 'boolean'
            WHEN 'int2' THEN 'smallint'
            WHEN 'int4' THEN 'integer'
            WHEN 'int8' THEN 'bigint'
            WHEN 'float4' THEN 'real'
            WHEN 'float8' THEN 'double precision'
            WHEN 'varchar' THEN 'character varying'
            WHEN 'text' THEN 'text'
            WHEN 'date' THEN 'date'
            WHEN 'time' THEN 'time without time zone'
            WHEN 'timestamp' THEN 'timestamp without time zone'
            WHEN 'numeric' THEN 'numeric'
            ELSE t.rsduck_physical_type
        END AS data_type,
        '' AS character_maximum_length,
        '' AS character_octet_length,
        CASE t.typname
            WHEN 'int2' THEN '16'
            WHEN 'int4' THEN '32'
            WHEN 'int8' THEN '64'
            WHEN 'float4' THEN '24'
            WHEN 'float8' THEN '53'
            ELSE ''
        END AS numeric_precision,
        CASE WHEN t.typname IN ('int2', 'int4', 'int8', 'float4', 'float8', 'numeric') THEN '2' ELSE '' END AS numeric_precision_radix,
        '' AS numeric_scale,
        CASE WHEN t.typname IN ('time', 'timestamp') THEN '6' ELSE '' END AS datetime_precision,
        '' AS interval_type,
        '' AS interval_precision,
        '' AS character_set_catalog,
        '' AS character_set_schema,
        '' AS character_set_name,
        '' AS collation_catalog,
        '' AS collation_schema,
        '' AS collation_name,
        '' AS domain_catalog,
        '' AS domain_schema,
        '' AS domain_name,
        current_database() AS udt_catalog,
        'pg_catalog' AS udt_schema,
        t.typname AS udt_name,
        '' AS scope_catalog,
        '' AS scope_schema,
        '' AS scope_name,
        '' AS maximum_cardinality,
        CAST(a.attnum AS VARCHAR) AS dtd_identifier,
        'NO' AS is_self_referencing,
        CASE WHEN a.attidentity <> '' THEN 'YES' ELSE 'NO' END AS is_identity,
        a.attidentity AS identity_generation,
        '' AS identity_start,
        '' AS identity_increment,
        '' AS identity_maximum,
        '' AS identity_minimum,
        'NO' AS identity_cycle,
        CASE WHEN a.attgenerated <> '' THEN 'ALWAYS' ELSE 'NEVER' END AS is_generated,
        '' AS generation_expression,
        'YES' AS is_updatable
    FROM rsduck_catalog.pg_attribute a
    JOIN rsduck_catalog.pg_class c ON c.oid = a.attrelid
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.pg_type t ON t.oid = a.atttypid
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.pg_attrdef def
      ON def.adrelid = a.attrelid AND def.adnum = a.attnum
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
      AND a.attisdropped = FALSE
    ORDER BY n.nspname, c.relname, a.attnum
    "
    .to_string()
}

pub(super) fn pg_type_sql() -> String {
    "
    SELECT
        CAST(t.oid AS VARCHAR) AS oid,
        t.typname,
        CAST(t.typnamespace AS VARCHAR) AS typnamespace,
        CAST(t.typowner AS VARCHAR) AS typowner,
        CAST(t.typlen AS VARCHAR) AS typlen,
        CASE WHEN t.typbyval THEN 't' ELSE 'f' END AS typbyval,
        t.typtype,
        t.typcategory,
        CASE WHEN t.typisdefined THEN 't' ELSE 'f' END AS typisdefined,
        CAST(t.typrelid AS VARCHAR) AS typrelid,
        CAST(t.typelem AS VARCHAR) AS typelem,
        CAST(t.typarray AS VARCHAR) AS typarray,
        t.rsduck_physical_type
    FROM rsduck_catalog.pg_type t
    LEFT JOIN rsduck_catalog.pg_class c ON c.oid = t.typrelid
    LEFT JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    WHERE t.typtype = 'b'
       OR (
           t.typtype = 'c'
           AND c.status IN ('active', 'unavailable')
           AND COALESCE(ext.visibility, 'user') = 'user'
       )
    ORDER BY t.oid
    "
    .to_string()
}

pub(super) fn pg_database_sql() -> String {
    "
    SELECT
        '1' AS oid,
        current_database() AS datname,
        '10' AS datdba,
        '6' AS encoding,
        'C' AS datcollate,
        'C' AS datctype,
        'f' AS datistemplate,
        't' AS datallowconn,
        '-1' AS datconnlimit,
        '0' AS datlastsysoid,
        '0' AS datfrozenxid,
        '0' AS datminmxid,
        '0' AS dattablespace,
        '' AS datacl,
        current_database() AS databasename,
        'admin' AS databaseowner,
        '' AS description,
        'UTF8' AS encodingname,
        'pg_default' AS spcname
    "
    .to_string()
}

pub(super) fn pg_user_sql() -> String {
    "
    SELECT
        u.username AS usename,
        CAST(u.user_id AS VARCHAR) AS usesysid,
        CASE WHEN EXISTS (
            SELECT 1
            FROM rsduck_catalog.rs_user_role ur
            JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id
            WHERE ur.user_id = u.user_id AND r.role_name = 'admin'
        ) THEN 't' ELSE 'f' END AS usecreatedb,
        CASE WHEN EXISTS (
            SELECT 1
            FROM rsduck_catalog.rs_user_role ur
            JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id
            WHERE ur.user_id = u.user_id AND r.role_name = 'admin'
        ) THEN 't' ELSE 'f' END AS usesuper,
        'f' AS userepl,
        '' AS passwd,
        '' AS valuntil,
        '' AS useconfig
    FROM rsduck_catalog.rs_user u
    WHERE u.status = 'active'
    ORDER BY u.username
    "
    .to_string()
}

pub(super) fn pg_roles_sql() -> String {
    "
    SELECT
        CAST(role_id AS VARCHAR) AS oid,
        role_name AS rolname,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolsuper,
        't' AS rolinherit,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolcreaterole,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolcreatedb,
        'f' AS rolcanlogin,
        'f' AS rolreplication,
        '-1' AS rolconnlimit,
        '' AS rolpassword,
        '' AS rolvaliduntil,
        CASE WHEN role_name = 'admin' THEN 't' ELSE 'f' END AS rolbypassrls,
        '' AS rolconfig
    FROM rsduck_catalog.rs_role
    ORDER BY role_name
    "
    .to_string()
}

pub(super) fn pg_settings_sql() -> String {
    let rows = pg_settings_rows()
        .into_iter()
        .map(|(name, setting)| {
            format!(
                "SELECT '{}' AS name, '{}' AS setting, '' AS unit, \
                 'Preset Options' AS category, '' AS short_desc, '' AS extra_desc, \
                 'internal' AS context, 'string' AS vartype, 'default' AS source, \
                 '' AS min_val, '' AS max_val, '' AS enumvals, '{}' AS boot_val, \
                 '{}' AS reset_val, '' AS sourcefile, '' AS sourceline, 'f' AS pending_restart",
                sql_string_literal(name),
                sql_string_literal(setting),
                sql_string_literal(setting),
                sql_string_literal(setting)
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n");
    format!("{rows}\nORDER BY name")
}

pub(super) fn pg_index_sql() -> String {
    "
    SELECT
        CAST(indexrelid AS VARCHAR) AS indexrelid,
        CAST(indrelid AS VARCHAR) AS indrelid,
        CAST(indnatts AS VARCHAR) AS indnatts,
        CAST(indnkeyatts AS VARCHAR) AS indnkeyatts,
        CASE WHEN indisunique THEN 't' ELSE 'f' END AS indisunique,
        CASE WHEN indisprimary THEN 't' ELSE 'f' END AS indisprimary,
        'f' AS indisclustered,
        CASE WHEN indisvalid THEN 't' ELSE 'f' END AS indisvalid,
        indkey,
        indexprs,
        indpred
    FROM rsduck_catalog.pg_index
    ORDER BY indexrelid
    "
    .to_string()
}

pub(super) fn pg_inherits_sql() -> String {
    "
    SELECT
        CAST(child_relid AS VARCHAR) AS inhrelid,
        CAST(parent_relid AS VARCHAR) AS inhparent,
        '1' AS inhseqno
    FROM rsduck_catalog.rs_partition
    WHERE status IN ('active', 'unavailable')
    ORDER BY parent_relid, child_relid
    "
    .to_string()
}

pub(super) fn pg_tablespace_sql() -> String {
    "
    SELECT
        '0' AS oid,
        'pg_default' AS spcname,
        '10' AS spcowner,
        '' AS spcacl,
        '' AS spcoptions
    "
    .to_string()
}

pub(super) fn pg_collation_sql() -> String {
    "
    SELECT
        '0' AS oid,
        '' AS collname,
        '0' AS collnamespace,
        '10' AS collowner,
        '' AS collprovider,
        'f' AS collisdeterministic,
        '-1' AS collencoding,
        '' AS collcollate,
        '' AS collctype,
        '' AS colliculocale,
        '' AS collicurules,
        '' AS collversion
    WHERE FALSE
    "
    .to_string()
}

pub(super) fn pg_sequence_sql() -> String {
    "
    SELECT
        '0' AS seqrelid,
        '20' AS seqtypid,
        '1' AS seqstart,
        '1' AS seqincrement,
        '9223372036854775807' AS seqmax,
        '1' AS seqmin,
        '1' AS seqcache,
        'f' AS seqcycle
    WHERE FALSE
    "
    .to_string()
}

pub(super) fn pg_constraint_sql() -> String {
    "
    SELECT
        CAST(con.oid AS VARCHAR) AS oid,
        con.conname,
        CAST(con.connamespace AS VARCHAR) AS connamespace,
        con.contype,
        CAST(con.conrelid AS VARCHAR) AS conrelid,
        CAST(con.conindid AS VARCHAR) AS conindid,
        con.conkey,
        CAST(con.confrelid AS VARCHAR) AS confrelid,
        con.confkey,
        CASE WHEN con.convalidated THEN 't' ELSE 'f' END AS convalidated,
        con.conbin,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        CASE con.contype
            WHEN 'p' THEN 'PRIMARY KEY'
            WHEN 'u' THEN 'UNIQUE'
            WHEN 'c' THEN 'CHECK'
            WHEN 'f' THEN 'FOREIGN KEY'
            ELSE con.contype
        END AS constraint_type
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    ORDER BY n.nspname, con.conname
    "
    .to_string()
}

pub(super) fn information_schema_table_constraints_sql() -> String {
    "
    SELECT
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        CASE con.contype
            WHEN 'p' THEN 'PRIMARY KEY'
            WHEN 'u' THEN 'UNIQUE'
            WHEN 'c' THEN 'CHECK'
            WHEN 'f' THEN 'FOREIGN KEY'
            ELSE con.contype
        END AS constraint_type,
        'NO' AS is_deferrable,
        'NO' AS initially_deferred,
        CASE WHEN con.convalidated THEN 'YES' ELSE 'NO' END AS enforced,
        CASE WHEN con.contype = 'u' THEN 'YES' ELSE '' END AS nulls_distinct
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    ORDER BY n.nspname, con.conname
    "
    .to_string()
}

pub(super) fn information_schema_key_column_usage_sql() -> String {
    "
    SELECT
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name,
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        CAST(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)) AS VARCHAR) AS ordinal_position,
        CASE WHEN con.contype = 'f'
             THEN CAST(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)) AS VARCHAR)
             ELSE ''
        END AS position_in_unique_constraint
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    JOIN rsduck_catalog.pg_attribute a
      ON a.attrelid = con.conrelid
     AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
    WHERE con.contype IN ('p', 'u', 'f')
      AND con.conkey <> ''
    ORDER BY tn.nspname, tc.relname, con.conname,
             list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR))
    "
    .to_string()
}

pub(super) fn information_schema_constraint_column_usage_sql() -> String {
    "
    SELECT
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.conrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    JOIN rsduck_catalog.pg_attribute a
      ON a.attrelid = con.conrelid
     AND COALESCE(list_position(string_split(con.conkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
    WHERE con.contype <> 'f'
      AND con.conkey <> ''
    UNION ALL
    SELECT
        current_database() AS table_catalog,
        tn.nspname AS table_schema,
        tc.relname AS table_name,
        a.attname AS column_name,
        current_database() AS constraint_catalog,
        n.nspname AS constraint_schema,
        con.conname AS constraint_name
    FROM rsduck_catalog.pg_constraint con
    JOIN rsduck_catalog.pg_namespace n ON n.oid = con.connamespace
    JOIN rsduck_catalog.pg_class tc ON tc.oid = con.confrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    JOIN rsduck_catalog.pg_attribute a
      ON a.attrelid = con.confrelid
     AND COALESCE(list_position(string_split(con.confkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
    WHERE con.contype = 'f'
      AND con.confkey <> ''
    ORDER BY table_schema, table_name, constraint_name, column_name
    "
    .to_string()
}

pub(super) fn pg_attrdef_sql() -> String {
    "
    SELECT
        CAST(d.oid AS VARCHAR) AS oid,
        CAST(d.adrelid AS VARCHAR) AS adrelid,
        CAST(d.adnum AS VARCHAR) AS adnum,
        d.adbin
    FROM rsduck_catalog.pg_attrdef d
    JOIN rsduck_catalog.pg_class c ON c.oid = d.adrelid
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    WHERE c.status IN ('active', 'unavailable')
      AND ext.visibility = 'user'
    ORDER BY d.adrelid, d.adnum
    "
    .to_string()
}

pub(super) fn pg_depend_sql() -> String {
    "
    SELECT
        CAST(classid AS VARCHAR) AS classid,
        CAST(objid AS VARCHAR) AS objid,
        CAST(objsubid AS VARCHAR) AS objsubid,
        CAST(refclassid AS VARCHAR) AS refclassid,
        CAST(refobjid AS VARCHAR) AS refobjid,
        CAST(refobjsubid AS VARCHAR) AS refobjsubid,
        deptype
    FROM rsduck_catalog.pg_depend
    ORDER BY classid, objid, refclassid, refobjid
    "
    .to_string()
}

pub(super) fn pg_description_sql() -> String {
    "
    SELECT
        CAST(objoid AS VARCHAR) AS objoid,
        CAST(classoid AS VARCHAR) AS classoid,
        CAST(objsubid AS VARCHAR) AS objsubid,
        description
    FROM rsduck_catalog.pg_description
    ORDER BY objoid, objsubid
    "
    .to_string()
}

pub(super) fn pg_views_sql() -> String {
    "
    SELECT
        n.nspname AS schemaname,
        c.relname AS viewname,
        COALESCE(u.username, 'unknown') AS viewowner,
        ext.generated_sql AS definition
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    LEFT JOIN rsduck_catalog.rs_user u ON u.user_id = c.relowner
    WHERE c.status = 'active'
      AND c.relkind = 'v'
      AND ext.visibility = 'user'
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

pub(super) fn information_schema_views_sql() -> String {
    "
    SELECT
        current_database() AS table_catalog,
        n.nspname AS table_schema,
        c.relname AS table_name,
        ext.generated_sql AS view_definition,
        'NONE' AS check_option,
        'NO' AS is_updatable,
        'NO' AS is_insertable_into,
        'NO' AS is_trigger_updatable,
        'NO' AS is_trigger_deletable,
        'NO' AS is_trigger_insertable_into
    FROM rsduck_catalog.pg_class c
    JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN rsduck_catalog.rs_relation_ext ext ON ext.relid = c.oid
    WHERE c.status = 'active'
      AND c.relkind = 'v'
      AND ext.visibility = 'user'
    ORDER BY n.nspname, c.relname
    "
    .to_string()
}

pub(super) fn pg_indexes_sql() -> String {
    "
    SELECT
        tn.nspname AS schemaname,
        tc.relname AS tablename,
        inx.relname AS indexname,
        '' AS tablespace,
        CASE WHEN i.indisunique THEN 'CREATE UNIQUE INDEX ' ELSE 'CREATE INDEX ' END ||
        inx.relname || ' ON ' || tn.nspname || '.' || tc.relname || ' (' ||
        COALESCE((
            SELECT string_agg(a.attname, ', ' ORDER BY list_position(string_split(i.indkey, ','), CAST(a.attnum AS VARCHAR)))
            FROM rsduck_catalog.pg_attribute a
            WHERE a.attrelid = i.indrelid
              AND COALESCE(list_position(string_split(i.indkey, ','), CAST(a.attnum AS VARCHAR)), 0) > 0
        ), '') || ')' AS indexdef
    FROM rsduck_catalog.pg_index i
    JOIN rsduck_catalog.pg_class inx ON inx.oid = i.indexrelid
    JOIN rsduck_catalog.pg_class tc ON tc.oid = i.indrelid
    JOIN rsduck_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
    ORDER BY tn.nspname, tc.relname, inx.relname
    "
    .to_string()
}

pub(super) fn pg_proc_sql() -> String {
    let rows = [
        (20001, "version", "25", "", 0),
        (20002, "current_database", "25", "", 0),
        (20003, "current_schema", "25", "", 0),
        (20004, "current_setting", "25", "25", 1),
        (20005, "format_type", "25", "20 23", 2),
        (20006, "pg_table_is_visible", "16", "20", 1),
        (20007, "pg_get_expr", "25", "25 20", 2),
        (20008, "pg_get_constraintdef", "25", "20", 1),
        (20009, "obj_description", "25", "20", 1),
        (20010, "col_description", "25", "20 23", 2),
        (20011, "pg_get_userbyid", "25", "20", 1),
        (20012, "has_database_privilege", "16", "25 25", 2),
        (20013, "has_schema_privilege", "16", "25 25", 2),
        (20014, "has_table_privilege", "16", "25 25", 2),
        (20015, "pg_backend_pid", "23", "", 0),
        (20016, "pg_is_in_recovery", "16", "", 0),
        (20017, "inet_server_addr", "25", "", 0),
        (20018, "inet_server_port", "23", "", 0),
    ];
    let sql = rows
        .into_iter()
        .map(|(oid, proname, prorettype, proargtypes, pronargs)| {
            format!(
                "SELECT '{oid}' AS oid, '{}' AS proname, '11' AS pronamespace, \
                 '10' AS proowner, '12' AS prolang, '1' AS procost, '0' AS prorows, \
                 '0' AS provariadic, '-' AS prosupport, 'f' AS prokind, 'f' AS prosecdef, \
                 'f' AS proleakproof, 'f' AS proisstrict, 'f' AS proretset, 's' AS provolatile, \
                 's' AS proparallel, '{pronargs}' AS pronargs, '0' AS pronargdefaults, \
                 '{prorettype}' AS prorettype, '{}' AS proargtypes, '' AS proallargtypes, \
                 '' AS proargmodes, '' AS proargnames, '' AS proargdefaults, '' AS protrftypes, \
                 '{}' AS prosrc, '' AS probin, '' AS prosqlbody, '' AS proconfig, '' AS proacl",
                sql_string_literal(proname),
                sql_string_literal(proargtypes),
                sql_string_literal(proname)
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n");
    format!("{sql}\nORDER BY proname")
}

pub(super) fn empty_pg_catalog_sql(sql: &str) -> Option<String> {
    if contains_from_table(sql, "pg_trigger") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '0' AS tgrelid,
                '' AS tgname,
                '0' AS tgfoid,
                '0' AS tgtype,
                '' AS tgenabled,
                'f' AS tgisinternal,
                '0' AS tgconstrrelid,
                '0' AS tgconstrindid,
                '0' AS tgconstraint,
                'f' AS tgdeferrable,
                'f' AS tginitdeferred,
                '0' AS tgnargs,
                '' AS tgattr,
                '' AS tgargs,
                '' AS tgqual,
                '' AS tgoldtable,
                '' AS tgnewtable
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_extension") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS extname,
                '0' AS extowner,
                '0' AS extnamespace,
                'f' AS extrelocatable,
                '' AS extversion,
                '' AS extconfig,
                '' AS extcondition
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_policy") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS polname,
                '0' AS polrelid,
                '' AS polcmd,
                'f' AS polpermissive,
                '' AS polroles,
                '' AS polqual,
                '' AS polwithcheck
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_matviews") {
        return Some(
            "
            SELECT
                '' AS schemaname,
                '' AS matviewname,
                '' AS matviewowner,
                '' AS tablespace,
                'f' AS hasindexes,
                'f' AS ispopulated,
                '' AS definition
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_sequences") {
        return Some(
            "
            SELECT
                '' AS schemaname,
                '' AS sequencename,
                '' AS sequenceowner,
                '' AS data_type,
                '' AS start_value,
                '' AS min_value,
                '' AS max_value,
                '' AS increment_by,
                'f' AS cycle,
                '' AS cache_size,
                '' AS last_value
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_foreign_table") {
        return Some(
            "
            SELECT
                '0' AS ftrelid,
                '0' AS ftserver,
                '' AS ftoptions
            WHERE FALSE
            "
            .to_string(),
        );
    }
    if contains_from_table(sql, "pg_foreign_server") {
        return Some(
            "
            SELECT
                '0' AS oid,
                '' AS srvname,
                '0' AS srvowner,
                '0' AS srvfdw,
                '' AS srvtype,
                '' AS srvversion,
                '' AS srvacl,
                '' AS srvoptions
            WHERE FALSE
            "
            .to_string(),
        );
    }
    None
}

pub(super) fn contains_from_table(sql: &str, table: &str) -> bool {
    let pg_catalog_table = format!("pg_catalog.{table}");
    let quoted_pg_catalog_table = format!("\"pg_catalog\".\"{table}\"");
    sql.contains(&format!(" from {table}"))
        || sql.contains(&format!(" from {pg_catalog_table}"))
        || sql.contains(&format!(" from {quoted_pg_catalog_table}"))
        || sql.contains(&format!(" join {table}"))
        || sql.contains(&format!(" join {pg_catalog_table}"))
        || sql.contains(&format!(" join {quoted_pg_catalog_table}"))
}

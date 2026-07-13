use super::*;

pub(crate) fn create_catalog_storage(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        CREATE SCHEMA IF NOT EXISTS rsduck_catalog;
        CREATE SCHEMA IF NOT EXISTS rsduck_internal;

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_catalog_version (
            id BIGINT PRIMARY KEY,
            schema_version BIGINT NOT NULL,
            snapshot_format_version BIGINT NOT NULL,
            catalog_epoch BIGINT NOT NULL,
            catalog_checksum VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_oid_alloc (
            id BIGINT PRIMARY KEY,
            next_oid BIGINT NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_catalog_journal (
            journal_id BIGINT PRIMARY KEY,
            catalog_epoch BIGINT NOT NULL,
            mutation_type VARCHAR NOT NULL,
            target_oid BIGINT NOT NULL,
            request_json VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            error_message VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            applied_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_schema (
            oid BIGINT PRIMARY KEY,
            nspname VARCHAR NOT NULL UNIQUE,
            nspowner BIGINT NOT NULL,
            nspacl VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_type (
            oid BIGINT PRIMARY KEY,
            typname VARCHAR NOT NULL,
            typnamespace BIGINT NOT NULL,
            typowner BIGINT NOT NULL,
            typlen INT NOT NULL,
            typbyval BOOLEAN NOT NULL,
            typtype VARCHAR NOT NULL,
            typcategory VARCHAR NOT NULL,
            typisdefined BOOLEAN NOT NULL,
            typrelid BIGINT NOT NULL,
            typelem BIGINT NOT NULL,
            typarray BIGINT NOT NULL,
            rsduck_physical_type VARCHAR NOT NULL,
            UNIQUE(typnamespace, typname)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_relation (
            oid BIGINT PRIMARY KEY,
            relname VARCHAR NOT NULL,
            relnamespace BIGINT NOT NULL,
            reltype BIGINT NOT NULL,
            relowner BIGINT NOT NULL,
            relkind VARCHAR NOT NULL,
            relpersistence VARCHAR NOT NULL,
            relnatts INT NOT NULL,
            reltuples DOUBLE NOT NULL,
            relhasindex BOOLEAN NOT NULL,
            relispartition BOOLEAN NOT NULL,
            relpartbound VARCHAR NOT NULL,
            reloptions VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            error_message VARCHAR NOT NULL,
            UNIQUE(relnamespace, relname)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_column (
            attrelid BIGINT NOT NULL,
            attname VARCHAR NOT NULL,
            atttypid BIGINT NOT NULL,
            attnum INT NOT NULL,
            atttypmod INT NOT NULL,
            attnotnull BOOLEAN NOT NULL,
            atthasdef BOOLEAN NOT NULL,
            attisdropped BOOLEAN NOT NULL,
            attidentity VARCHAR NOT NULL,
            attgenerated VARCHAR NOT NULL,
            attoptions VARCHAR NOT NULL,
            PRIMARY KEY(attrelid, attnum)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_column_default (
            oid BIGINT PRIMARY KEY,
            adrelid BIGINT NOT NULL,
            adnum INT NOT NULL,
            adbin VARCHAR NOT NULL,
            UNIQUE(adrelid, adnum)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_constraint (
            oid BIGINT PRIMARY KEY,
            conname VARCHAR NOT NULL,
            connamespace BIGINT NOT NULL,
            contype VARCHAR NOT NULL,
            conrelid BIGINT NOT NULL,
            conindid BIGINT NOT NULL,
            conkey VARCHAR NOT NULL,
            confrelid BIGINT NOT NULL,
            confkey VARCHAR NOT NULL,
            convalidated BOOLEAN NOT NULL,
            conbin VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_index (
            indexrelid BIGINT PRIMARY KEY,
            indrelid BIGINT NOT NULL,
            indnatts INT NOT NULL,
            indnkeyatts INT NOT NULL,
            indisunique BOOLEAN NOT NULL,
            indisprimary BOOLEAN NOT NULL,
            indisvalid BOOLEAN NOT NULL,
            indkey VARCHAR NOT NULL,
            indexprs VARCHAR NOT NULL,
            indpred VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_vector_index (
            indexrelid BIGINT PRIMARY KEY,
            vector_space VARCHAR NOT NULL UNIQUE,
            embedding_model VARCHAR NOT NULL,
            model_version VARCHAR NOT NULL,
            dimension INT NOT NULL,
            metric VARCHAR NOT NULL,
            m INT NOT NULL,
            m0 INT NOT NULL,
            ef_construction INT NOT NULL,
            default_ef_search INT NOT NULL,
            definition_version BIGINT NOT NULL,
            generation BIGINT NOT NULL,
            extension_version VARCHAR NOT NULL,
            build_status VARCHAR NOT NULL,
            vector_count BIGINT NOT NULL,
            built_at TIMESTAMP,
            updated_at TIMESTAMP NOT NULL,
            error_message VARCHAR NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_dependency (
            classid BIGINT NOT NULL,
            objid BIGINT NOT NULL,
            objsubid INT NOT NULL,
            refclassid BIGINT NOT NULL,
            refobjid BIGINT NOT NULL,
            refobjsubid INT NOT NULL,
            deptype VARCHAR NOT NULL,
            PRIMARY KEY(classid, objid, objsubid, refclassid, refobjid, refobjsubid)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_comment (
            objoid BIGINT NOT NULL,
            classoid BIGINT NOT NULL,
            objsubid INT NOT NULL,
            description VARCHAR NOT NULL,
            PRIMARY KEY(objoid, classoid, objsubid)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_relation_ext (
            relid BIGINT PRIMARY KEY,
            managed_kind VARCHAR NOT NULL,
            storage_mode VARCHAR NOT NULL,
            visibility VARCHAR NOT NULL,
            partition_key VARCHAR NOT NULL,
            partition_key_type VARCHAR NOT NULL,
            partition_unit VARCHAR NOT NULL,
            retention_count INT NOT NULL,
            generated_sql VARCHAR NOT NULL,
            properties_json VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_partition (
            parent_relid BIGINT NOT NULL,
            child_relid BIGINT NOT NULL,
            partition_value VARCHAR NOT NULL,
            partition_unit VARCHAR NOT NULL,
            lower_bound TIMESTAMP,
            upper_bound TIMESTAMP,
            is_null_partition BOOLEAN NOT NULL,
            status VARCHAR NOT NULL,
            row_count BIGINT NOT NULL,
            min_ts TIMESTAMP,
            max_ts TIMESTAMP,
            checksum VARCHAR NOT NULL,
            created_at TIMESTAMP NOT NULL,
            activated_at TIMESTAMP,
            dropped_at TIMESTAMP,
            error_message VARCHAR NOT NULL,
            PRIMARY KEY(parent_relid, child_relid),
            UNIQUE(parent_relid, partition_value)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_user (
            user_id BIGINT PRIMARY KEY,
            username VARCHAR NOT NULL UNIQUE,
            password_hash VARCHAR NOT NULL,
            password_algo VARCHAR NOT NULL,
            mysql_auth_plugin VARCHAR NOT NULL,
            mysql_auth_string VARCHAR NOT NULL,
            status VARCHAR NOT NULL,
            is_builtin BOOLEAN NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL,
            last_login_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_role (
            role_id BIGINT PRIMARY KEY,
            role_name VARCHAR NOT NULL UNIQUE,
            description VARCHAR NOT NULL,
            is_builtin BOOLEAN NOT NULL,
            created_at TIMESTAMP NOT NULL,
            updated_at TIMESTAMP NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_user_role (
            user_id BIGINT NOT NULL,
            role_id BIGINT NOT NULL,
            granted_by BIGINT NOT NULL,
            created_at TIMESTAMP NOT NULL,
            PRIMARY KEY(user_id, role_id)
        );

        CREATE TABLE IF NOT EXISTS rsduck_catalog.rs_privilege (
            privilege_id BIGINT PRIMARY KEY,
            principal_type VARCHAR NOT NULL,
            principal_id BIGINT NOT NULL,
            object_type VARCHAR NOT NULL,
            object_id BIGINT NOT NULL,
            action VARCHAR NOT NULL,
            granted_by BIGINT NOT NULL,
            created_at TIMESTAMP NOT NULL,
            UNIQUE(principal_type, principal_id, object_type, object_id, action)
        );
        ",
    )
    .map_err(|e| format!("create catalog storage failed: {e}"))?;
    Ok(())
}

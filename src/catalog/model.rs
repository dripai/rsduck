use chrono::NaiveDateTime;

pub(super) const CATALOG_VERSION: i64 = 1;

pub(super) const ADMIN_USER_ID: i64 = 10;
pub(super) const PG_CATALOG_NS: i64 = 11;
pub(super) const INFORMATION_SCHEMA_NS: i64 = 12;
pub(super) const RSDUCK_CATALOG_NS: i64 = 13;
pub(super) const RSDUCK_INTERNAL_NS: i64 = 14;
pub(super) const MAIN_NS: i64 = 15;

pub(super) const ROLE_ADMIN_ID: i64 = 20;
pub(super) const ROLE_OPERATOR_ID: i64 = 21;
pub(super) const ROLE_DDL_ID: i64 = 22;
pub(super) const ROLE_WRITER_ID: i64 = 23;
pub(super) const ROLE_READER_ID: i64 = 24;

pub(super) const FIRST_USER_OID: i64 = 10_000;
pub(super) const PG_CLASS_CLASSOID: i64 = 1259;
pub(super) const PG_CONSTRAINT_CLASSOID: i64 = 2606;
pub(super) const PG_NAMESPACE_CLASSOID: i64 = 2615;
pub(super) const FNV64_OFFSET: u64 = 0xcbf29ce484222325;
pub(super) const FNV64_PRIME: u64 = 0x00000100000001b3;
pub(super) const AUTH_FAILED: &str = "invalid username or password";

#[derive(Debug, Clone)]
pub(super) struct CatalogColumn {
    pub(super) name: String,
    pub(super) pg_type_oid: i64,
    pub(super) attnum: i32,
    pub(super) not_null: bool,
    pub(super) default_expr: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct RelationMeta {
    pub(super) oid: i64,
    pub(super) reltype: i64,
    pub(super) relkind: String,
    pub(super) relispartition: bool,
}

#[derive(Debug, Clone)]
pub(super) struct RelationAccessMeta {
    pub(super) oid: i64,
    pub(super) status: String,
    pub(super) error_message: String,
}

#[derive(Debug, Clone)]
pub(super) struct ManagedPartitionCreate {
    pub(super) base_sql: String,
    pub(super) partition_key: String,
    pub(super) partition_unit: String,
    pub(super) retention_count: i32,
}

#[derive(Debug, Clone)]
pub(super) struct PartitionedRelation {
    pub(super) oid: i64,
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) partition_key: String,
    pub(super) partition_key_type: String,
    pub(super) partition_unit: String,
    pub(super) retention_count: i32,
    pub(super) columns: Vec<CatalogColumn>,
}

#[derive(Debug, Clone)]
pub(super) struct PartitionRoute {
    pub(super) partition_value: String,
    pub(super) route_ts: Option<NaiveDateTime>,
}

#[derive(Debug, Clone)]
pub(super) struct PartitionBounds {
    pub(super) value: String,
    pub(super) lower_bound: NaiveDateTime,
    pub(super) upper_bound: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct SessionPrincipal {
    pub user_id: i64,
    pub username: String,
    pub roles: Vec<String>,
}

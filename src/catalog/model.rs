use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use duckdb::{
    types::{TimeUnit, ValueRef},
    Connection,
};
use rand_core::OsRng;
use sqlparser::ast::{
    Action, AlterTable, AlterTableOperation, AlterUser, ColumnOption, CommentObject, CreateIndex,
    CreateRole, CreateTable, CreateUser, CreateView, Expr, ForeignKeyConstraint, Grant,
    GrantObjects, GranteeName, GranteesType, Insert, ObjectName, ObjectNamePart, ObjectType,
    Privileges, Revoke, SchemaName, SetExpr, Statement, TableConstraint, TableObject, Value,
};
use sqlparser::dialect::{DuckDbDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use tracing::{info, warn};

const CATALOG_VERSION: i64 = 1;

const ADMIN_USER_ID: i64 = 10;
const PG_CATALOG_NS: i64 = 11;
const INFORMATION_SCHEMA_NS: i64 = 12;
const RSDUCK_CATALOG_NS: i64 = 13;
const RSDUCK_INTERNAL_NS: i64 = 14;
const MAIN_NS: i64 = 15;

const ROLE_ADMIN_ID: i64 = 20;
const ROLE_OPERATOR_ID: i64 = 21;
const ROLE_DDL_ID: i64 = 22;
const ROLE_WRITER_ID: i64 = 23;
const ROLE_READER_ID: i64 = 24;

const FIRST_USER_OID: i64 = 10_000;
const PG_CLASS_CLASSOID: i64 = 1259;
const PG_CONSTRAINT_CLASSOID: i64 = 2606;
const PG_NAMESPACE_CLASSOID: i64 = 2615;
const FNV64_OFFSET: u64 = 0xcbf29ce484222325;
const FNV64_PRIME: u64 = 0x00000100000001b3;
const AUTH_FAILED: &str = "invalid username or password";

#[derive(Debug, Clone)]
struct CatalogColumn {
    name: String,
    pg_type_oid: i64,
    attnum: i32,
    not_null: bool,
    default_expr: Option<String>,
}

#[derive(Debug, Clone)]
struct RelationMeta {
    oid: i64,
    reltype: i64,
    relkind: String,
    relispartition: bool,
}

#[derive(Debug, Clone)]
struct RelationAccessMeta {
    oid: i64,
    status: String,
    error_message: String,
}

#[derive(Debug, Clone)]
struct ManagedPartitionCreate {
    base_sql: String,
    partition_key: String,
    partition_unit: String,
    retention_count: i32,
}

#[derive(Debug, Clone)]
struct PartitionedRelation {
    oid: i64,
    schema: String,
    name: String,
    partition_key: String,
    partition_key_type: String,
    partition_unit: String,
    retention_count: i32,
    columns: Vec<CatalogColumn>,
}

#[derive(Debug, Clone)]
struct PartitionRoute {
    partition_value: String,
    route_ts: Option<NaiveDateTime>,
}

#[derive(Debug, Clone)]
struct PartitionBounds {
    value: String,
    lower_bound: NaiveDateTime,
    upper_bound: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct SessionPrincipal {
    pub user_id: i64,
    pub username: String,
    pub roles: Vec<String>,
}


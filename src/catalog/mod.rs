use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};
use duckdb::{
    types::{TimeUnit, ValueRef},
    Connection,
};
use rand_core::OsRng;
use sqlparser::ast::{
    Action, AlterColumnOperation, AlterTable, AlterTableOperation, AlterUser, ColumnOption,
    CommentObject, CreateIndex, CreateRole, CreateTable, CreateUser, CreateView, Expr,
    ForeignKeyConstraint, Grant, GrantObjects, GranteeName, GranteesType, Ident, Insert,
    ObjectName, ObjectNamePart, ObjectType, Privileges, Revoke, SchemaName, SetExpr, Statement,
    TableConstraint, TableObject, Value,
};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use tracing::{info, warn};

mod auth;
mod bootstrap;
mod checksum;
mod guard;
mod journal;
mod lookup;
mod model;
mod mutation;
mod oid;
mod partition;
mod recovery;
mod sql_util;
mod storage;
mod type_mapping;

pub(crate) use self::auth::*;
pub(crate) use self::bootstrap::*;
#[allow(unused_imports)]
pub(crate) use self::checksum::*;
pub(crate) use self::guard::*;
#[allow(unused_imports)]
pub(crate) use self::journal::*;
#[allow(unused_imports)]
pub(crate) use self::lookup::*;
pub(crate) use self::model::*;
pub(crate) use self::mutation::*;
#[allow(unused_imports)]
pub(crate) use self::oid::*;
#[allow(unused_imports)]
pub(crate) use self::partition::*;
pub(crate) use self::recovery::*;
#[allow(unused_imports)]
pub(crate) use self::sql_util::*;
#[allow(unused_imports)]
pub(crate) use self::storage::*;
#[allow(unused_imports)]
pub(crate) use self::type_mapping::*;

#[cfg(test)]
mod tests;

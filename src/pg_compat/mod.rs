mod catalog;
mod function;
mod rewrite;
mod settings;
mod show;

use crate::db::SqlResult;

#[allow(unused_imports)]
use self::catalog::*;
#[allow(unused_imports)]
use self::function::*;
#[allow(unused_imports)]
use self::rewrite::*;
#[allow(unused_imports)]
use self::settings::*;
#[allow(unused_imports)]
use self::show::*;

const PG_NAMESPACE_CLASSOID: i64 = 2615;

pub fn compat_result(sql: &str, current_user: &str) -> Option<SqlResult> {
    let normalized = normalize_sql(sql);
    pg_set_result(&normalized)
        .or_else(|| pg_show_result(&normalized))
        .or_else(|| pg_scalar_result(&normalized, current_user))
        .or_else(|| pg_database_legacy_result(&normalized))
}

pub fn rewrite_sql(sql: &str) -> Option<String> {
    let normalized = normalize_sql(sql);
    if let Some(sql) = rewrite_show_partitions_sql(sql) {
        return Some(sql);
    }
    if !normalized.starts_with("select ") && !normalized.starts_with("with ") {
        return None;
    }

    if let Some(sql) = catalog_scalar_function_sql(sql, &normalized) {
        return Some(sql);
    }

    let rewritten = rewrite_catalog_relation_references(sql)?;
    let rewritten = rewrite_catalog_function_calls(&rewritten).unwrap_or(rewritten);
    let rewritten = rewrite_pg_any_membership(&rewritten).unwrap_or(rewritten);
    Some(rewrite_pg_type_casts(&rewritten).unwrap_or(rewritten))
}

#[cfg(test)]
mod tests;

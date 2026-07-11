use sqlparser::ast::{LimitClause, Statement};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SqlRoute {
    Read,
    Write,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SqlRouteDecision {
    pub route: SqlRoute,
    pub command: String,
}

pub fn route_sql(sql: &str) -> Result<SqlRouteDecision, String> {
    if crate::catalog::looks_like_managed_partition_create(sql) {
        return Ok(SqlRouteDecision {
            route: SqlRoute::Write,
            command: "CREATE".to_string(),
        });
    }

    if looks_like_comment_on(sql) {
        return Ok(SqlRouteDecision {
            route: SqlRoute::Write,
            command: "COMMENT".to_string(),
        });
    }

    if looks_like_drop_role_cascade(sql) {
        return Ok(SqlRouteDecision {
            route: SqlRoute::Write,
            command: "DROP".to_string(),
        });
    }

    if crate::catalog::looks_like_show_partitions(sql) {
        return Ok(SqlRouteDecision {
            route: SqlRoute::Read,
            command: "SHOW".to_string(),
        });
    }

    if crate::mysql_compat::is_show_table_detail(sql) {
        return Ok(SqlRouteDecision {
            route: SqlRoute::Read,
            command: "SHOW".to_string(),
        });
    }

    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("sql parse failed: {e}"))?;

    if statements.len() != 1 {
        return Err(format!(
            "only one SQL statement is supported, got {}",
            statements.len()
        ));
    }

    let statement = &statements[0];
    Ok(SqlRouteDecision {
        route: statement_route(statement),
        command: statement_command(statement).to_string(),
    })
}

fn looks_like_comment_on(sql: &str) -> bool {
    let normalized = sql.trim_start().to_ascii_lowercase();
    normalized.starts_with("comment on ")
}

fn looks_like_drop_role_cascade(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut words = trimmed.split_ascii_whitespace();
    matches!(
        (
            words.next(),
            words.next(),
            trimmed.split_ascii_whitespace().last(),
        ),
        (Some(drop), Some(role), Some(cascade))
            if drop.eq_ignore_ascii_case("drop")
                && role.eq_ignore_ascii_case("role")
                && cascade.eq_ignore_ascii_case("cascade")
    )
}

pub fn is_pageable_sql(sql: &str) -> Result<bool, String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("sql parse failed: {e}"))?;

    if statements.len() != 1 {
        return Ok(false);
    }

    Ok(matches!(statements.first(), Some(Statement::Query(_))))
}

pub fn has_top_level_limit_or_offset(sql: &str) -> Result<bool, String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("sql parse failed: {e}"))?;

    let Some(Statement::Query(query)) = statements.first() else {
        return Ok(false);
    };
    if statements.len() != 1 {
        return Ok(false);
    }

    Ok(query.fetch.is_some()
        || matches!(
            &query.limit_clause,
            Some(LimitClause::LimitOffset { limit, offset, .. })
                if limit.is_some() || offset.is_some()
        )
        || matches!(
            &query.limit_clause,
            Some(LimitClause::OffsetCommaLimit { .. })
        ))
}

fn statement_route(statement: &Statement) -> SqlRoute {
    match statement {
        Statement::Query(_)
        | Statement::ExplainTable { .. }
        | Statement::Fetch { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCharset(_)
        | Statement::ShowObjects(_)
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowCollation { .. }
        | Statement::ExportData(_) => SqlRoute::Read,
        Statement::Explain {
            analyze, statement, ..
        } => {
            if *analyze {
                statement_route(statement)
            } else {
                SqlRoute::Read
            }
        }
        Statement::Copy { to, .. } => {
            if *to {
                SqlRoute::Read
            } else {
                SqlRoute::Write
            }
        }
        Statement::Pragma { value, .. } => {
            if value.is_some() {
                SqlRoute::Write
            } else {
                SqlRoute::Read
            }
        }
        _ => SqlRoute::Write,
    }
}

fn statement_command(statement: &Statement) -> &'static str {
    match statement {
        Statement::Analyze(_) => "ANALYZE",
        Statement::Set(_) => "SET",
        Statement::Truncate(_) => "TRUNCATE",
        Statement::Query(_) => "SELECT",
        Statement::Insert(_) => "INSERT",
        Statement::Install { .. } => "INSTALL",
        Statement::Load { .. } => "LOAD",
        Statement::Directory { .. } => "LOAD",
        Statement::Call(_) => "CALL",
        Statement::Copy { .. } => "COPY",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::CreateView(_) => "CREATE",
        Statement::CreateTable(_) => "CREATE",
        Statement::CreateUser(_) => "CREATE",
        Statement::CreateVirtualTable { .. } => "CREATE",
        Statement::CreateIndex(_) => "CREATE",
        Statement::CreateRole(_) => "CREATE",
        Statement::CreateSecret { .. } => "CREATE",
        Statement::CreateServer(_) => "CREATE",
        Statement::CreatePolicy(_) => "CREATE",
        Statement::CreateConnector(_) => "CREATE",
        Statement::CreateOperator(_) => "CREATE",
        Statement::CreateOperatorFamily(_) => "CREATE",
        Statement::CreateOperatorClass(_) => "CREATE",
        Statement::AlterTable(_) => "ALTER",
        Statement::AlterSchema(_) => "ALTER",
        Statement::AlterIndex { .. } => "ALTER",
        Statement::AlterView { .. } => "ALTER",
        Statement::AlterUser(_) => "ALTER",
        Statement::AlterFunction(_) => "ALTER",
        Statement::AlterType(_) => "ALTER",
        Statement::AlterCollation(_) => "ALTER",
        Statement::AlterOperator(_) => "ALTER",
        Statement::AlterOperatorFamily(_) => "ALTER",
        Statement::AlterOperatorClass(_) => "ALTER",
        Statement::AlterRole { .. } => "ALTER",
        Statement::AlterPolicy(_) => "ALTER",
        Statement::AlterConnector { .. } => "ALTER",
        Statement::AlterSession { .. } => "ALTER",
        Statement::AttachDatabase { .. } => "ATTACH",
        Statement::AttachDuckDBDatabase { .. } => "ATTACH",
        Statement::DetachDuckDBDatabase { .. } => "DETACH",
        Statement::Drop { .. }
        | Statement::DropFunction(_)
        | Statement::DropDomain(_)
        | Statement::DropProcedure { .. }
        | Statement::DropSecret { .. }
        | Statement::DropPolicy(_)
        | Statement::DropConnector { .. }
        | Statement::DropExtension(_)
        | Statement::DropOperator(_)
        | Statement::DropOperatorFamily(_)
        | Statement::DropOperatorClass(_) => "DROP",
        Statement::CreateExtension(_)
        | Statement::CreateCollation(_)
        | Statement::CreateSchema { .. }
        | Statement::CreateDatabase { .. }
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::CreateProcedure { .. }
        | Statement::CreateMacro { .. }
        | Statement::CreateStage { .. }
        | Statement::CreateSequence { .. }
        | Statement::CreateDomain(_)
        | Statement::CreateType { .. } => "CREATE",
        Statement::ExplainTable { .. } | Statement::Explain { .. } => "EXPLAIN",
        Statement::Merge(_) => "MERGE",
        Statement::Pragma { .. } => "PRAGMA",
        Statement::Cache { .. } => "CACHE",
        Statement::UNCache { .. } => "UNCACHE",
        Statement::Use(_) => "USE",
        Statement::StartTransaction { begin, .. } => {
            if *begin {
                "BEGIN"
            } else {
                "START"
            }
        }
        Statement::Commit { .. } => "COMMIT",
        Statement::Rollback { .. } => "ROLLBACK",
        Statement::Grant(_) => "GRANT",
        Statement::Deny(_) => "DENY",
        Statement::Revoke(_) => "REVOKE",
        Statement::ExportData(_) => "EXPORT",
        Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCharset(_)
        | Statement::ShowObjects(_)
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowCollation { .. } => "SHOW",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::{has_top_level_limit_or_offset, route_sql, SqlRoute};

    #[test]
    fn routes_select_and_show_to_read() {
        assert_eq!(
            route_sql("SELECT * FROM kline_day").unwrap().route,
            SqlRoute::Read
        );
        assert_eq!(route_sql("SHOW TABLES").unwrap().route, SqlRoute::Read);
        assert_eq!(
            route_sql("SHOW TABLE kline_day").unwrap().route,
            SqlRoute::Read
        );
        assert_eq!(
            route_sql("SHOW PARTITIONS FROM kline_day").unwrap().route,
            SqlRoute::Read
        );
        assert_eq!(route_sql("SHOW ALL TABLES").unwrap().route, SqlRoute::Read);
        assert_eq!(
            route_sql("DESCRIBE kline_day").unwrap().route,
            SqlRoute::Read
        );
    }

    #[test]
    fn routes_ddl_and_dml_to_write() {
        assert_eq!(
            route_sql("CREATE TABLE t(id INTEGER)").unwrap().route,
            SqlRoute::Write
        );
        let comment = route_sql("COMMENT ON TABLE quotes IS 'quotes table'").unwrap();
        assert_eq!(comment.route, SqlRoute::Write);
        assert_eq!(comment.command, "COMMENT");
        assert_eq!(
            route_sql("CREATE USER alice PASSWORD='pw'")
                .unwrap()
                .command,
            "CREATE"
        );
        assert_eq!(
            route_sql("ALTER USER alice PASSWORD 'newpw'")
                .unwrap()
                .command,
            "ALTER"
        );
        assert_eq!(
            route_sql("DROP ROLE analyst CASCADE").unwrap().route,
            SqlRoute::Write
        );
        assert_eq!(
            route_sql(
                "CREATE TABLE logs(id BIGINT, access_time TIMESTAMP)
                 PARTITION BY RANGE (access_time)
                 WITH (partition_unit = 'day', retention = '30')"
            )
            .unwrap()
            .route,
            SqlRoute::Write
        );
        assert_eq!(
            route_sql("INSERT INTO t VALUES (1)").unwrap().route,
            SqlRoute::Write
        );
        assert_eq!(
            route_sql("UPDATE t SET id = 2").unwrap().route,
            SqlRoute::Write
        );
        assert_eq!(
            route_sql("DELETE FROM t WHERE id = 2").unwrap().route,
            SqlRoute::Write
        );
    }

    #[test]
    fn routes_with_queries_to_read() {
        assert_eq!(
            route_sql("WITH x AS (SELECT 1) SELECT * FROM x")
                .unwrap()
                .route,
            SqlRoute::Read
        );
    }

    #[test]
    fn only_select_queries_are_pageable() {
        assert!(super::is_pageable_sql("SELECT * FROM kline_day").unwrap());
        assert!(super::is_pageable_sql("WITH x AS (SELECT 1) SELECT * FROM x").unwrap());
        assert!(!super::is_pageable_sql("SHOW TABLES").unwrap());
        assert!(!super::is_pageable_sql("DESCRIBE kline_day").unwrap());
    }

    #[test]
    fn detects_only_top_level_limit_or_offset() {
        assert!(has_top_level_limit_or_offset("SELECT * FROM kline_day LIMIT 10").unwrap());
        assert!(has_top_level_limit_or_offset("SELECT * FROM kline_day OFFSET 10").unwrap());
        assert!(
            has_top_level_limit_or_offset("SELECT * FROM kline_day LIMIT 10 OFFSET 20").unwrap()
        );
        assert!(
            has_top_level_limit_or_offset("SELECT * FROM kline_day FETCH FIRST 10 ROWS ONLY")
                .unwrap()
        );
        assert!(!has_top_level_limit_or_offset(
            "SELECT * FROM (SELECT * FROM kline_day LIMIT 10) t"
        )
        .unwrap());
        assert!(!has_top_level_limit_or_offset("SHOW TABLES").unwrap());
    }

    #[test]
    fn rejects_multi_statement_sql() {
        let err = route_sql("SELECT 1; SELECT 2").unwrap_err();
        assert!(err.contains("only one SQL statement"));
    }

    #[test]
    fn handles_leading_comments() {
        assert_eq!(
            route_sql("-- route comment\nSELECT 1").unwrap().route,
            SqlRoute::Read
        );
    }

    #[test]
    fn routes_copy_direction() {
        assert_eq!(
            route_sql("COPY kline_day TO 'out.parquet'").unwrap().route,
            SqlRoute::Read
        );
        assert_eq!(
            route_sql("COPY kline_day FROM 'in.parquet'").unwrap().route,
            SqlRoute::Write
        );
    }
}

use sqlparser::ast::Statement;
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

pub fn is_pageable_sql(sql: &str) -> Result<bool, String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("sql parse failed: {e}"))?;

    if statements.len() != 1 {
        return Ok(false);
    }

    Ok(matches!(statements.first(), Some(Statement::Query(_))))
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
    use super::{route_sql, SqlRoute};

    #[test]
    fn routes_select_and_show_to_read() {
        assert_eq!(
            route_sql("SELECT * FROM kline_day").unwrap().route,
            SqlRoute::Read
        );
        assert_eq!(route_sql("SHOW TABLES").unwrap().route, SqlRoute::Read);
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

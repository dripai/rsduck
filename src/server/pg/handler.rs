use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DescribePortalResponse, DescribeStatementResponse, FieldFormat, Response,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, Type, DEFAULT_NAME};
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::extendedquery::{Parse, ParseComplete};
use pgwire::messages::PgWireBackendMessage;
use tracing::debug;

use crate::db::{DbHandle, SqlColumn, SqlParam, SqlTypedResult};

use super::auth::RsduckServerParameterProvider;
use super::codec::{exec_to_response, fields_for_columns, pg_api_error, query_to_response};
use super::params::{
    dummy_sql_param_for_type, infer_parameter_types, infer_statement_parameter_types,
    sql_params_from_portal,
};
use super::session::{session_database, session_user};

pub(super) struct DuckdbProcessor {
    pub(super) db: DbHandle,
    pub(super) query_parser: Arc<NoopQueryParser>,
    pub(super) parameters: Arc<RsduckServerParameterProvider>,
}

impl DuckdbProcessor {
    pub(super) fn new(db: DbHandle) -> Self {
        Self {
            db,
            query_parser: Arc::new(NoopQueryParser::new()),
            parameters: Arc::new(RsduckServerParameterProvider),
        }
    }
}

async fn execute_pg_sql(
    db: &DbHandle,
    sql: String,
    current_user: &str,
    params: Vec<SqlParam>,
) -> Result<SqlTypedResult, String> {
    db.execute_typed_sql_with_params_as(current_user.to_string(), sql, params)
        .await
        .map_err(|e| e.to_string())
}

async fn describe_pg_sql(
    db: &DbHandle,
    sql: String,
    current_user: &str,
    params: Vec<SqlParam>,
) -> Result<Vec<SqlColumn>, String> {
    db.describe_sql_with_params_as(current_user.to_string(), sql, params)
        .await
        .map_err(|e| e.to_string())
}

#[async_trait]
impl SimpleQueryHandler for DuckdbProcessor {
    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = query.trim().to_string();
        if sql.is_empty() {
            debug!(target: "rsduck::pg", protocol = "simple", "PG empty query");
            return Ok(vec![Response::EmptyQuery]);
        }

        let current_user = session_user(client);
        let current_database = session_database(client);
        debug!(
            target: "rsduck::pg",
            protocol = "simple",
            user = %current_user,
            database = %current_database,
            sql = %sql,
            "PG query"
        );
        match execute_pg_sql(&self.db, sql.clone(), current_user, Vec::new()).await {
            Ok(SqlTypedResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                Ok(vec![query_to_response(columns, rows, FieldFormat::Text)?])
            }
            Ok(SqlTypedResult::Execute {
                command,
                affected_rows,
            }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    command = %command,
                    affected_rows,
                    "PG execute result"
                );
                Ok(vec![exec_to_response(&command, affected_rows)])
            }
            Err(e) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    sql = %sql,
                    error = %e,
                    "PG query failed"
                );
                Err(pg_api_error(e))
            }
        }
    }
}

#[async_trait]
impl ExtendedQueryHandler for DuckdbProcessor {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn on_parse<C>(&self, client: &mut C, message: Parse) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let provided_types = message
            .type_oids
            .iter()
            .map(|oid| Type::from_oid(*oid).unwrap_or(Type::UNKNOWN))
            .collect::<Vec<_>>();
        let parameter_types = infer_parameter_types(&message.query, &provided_types);
        let id = message
            .name
            .clone()
            .unwrap_or_else(|| DEFAULT_NAME.to_string());
        let statement = StoredStatement::new(id, message.query, parameter_types);
        client.portal_store().put_statement(Arc::new(statement));
        client
            .send(PgWireBackendMessage::ParseComplete(ParseComplete::new()))
            .await?;
        Ok(())
    }

    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        statement: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %session_user(client),
            database = %session_database(client),
            sql = %statement.statement,
            "PG describe statement"
        );
        let parameter_types = infer_statement_parameter_types(statement);
        let describe_params = parameter_types
            .iter()
            .map(dummy_sql_param_for_type)
            .collect();
        let fields = describe_pg_sql(
            &self.db,
            statement.statement.to_string(),
            session_user(client),
            describe_params,
        )
        .await
        .map(|columns| fields_for_columns(columns, FieldFormat::Binary))
        .map_err(pg_api_error)?;
        Ok(DescribeStatementResponse::new(parameter_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %session_user(client),
            database = %session_database(client),
            sql = %portal.statement.statement,
            "PG describe portal"
        );
        let params = sql_params_from_portal(portal).map_err(pg_api_error)?;
        let fields = describe_pg_sql(
            &self.db,
            portal.statement.statement.to_string(),
            session_user(client),
            params,
        )
        .await
        .map(|columns| fields_for_columns(columns, FieldFormat::Binary))
        .map_err(pg_api_error)?;
        Ok(DescribePortalResponse::new(fields))
    }

    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let sql = portal.statement.statement.to_string();

        let current_user = session_user(client);
        let current_database = session_database(client);
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %current_user,
            database = %current_database,
            sql = %sql,
            "PG query"
        );
        let params = sql_params_from_portal(portal).map_err(pg_api_error)?;
        match execute_pg_sql(&self.db, sql.clone(), current_user, params).await {
            Ok(SqlTypedResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                query_to_response(columns, rows, FieldFormat::Binary)
            }
            Ok(SqlTypedResult::Execute {
                command,
                affected_rows,
            }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    command = %command,
                    affected_rows,
                    "PG execute result"
                );
                Ok(exec_to_response(&command, affected_rows))
            }
            Err(e) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    sql = %sql,
                    error = %e,
                    "PG query failed"
                );
                Err(pg_api_error(e))
            }
        }
    }
}

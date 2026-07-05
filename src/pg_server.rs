use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Sink, SinkExt};
use pgwire::api::auth::{
    finish_authentication, save_startup_parameters_to_metadata, ServerParameterProvider,
    StartupHandler,
};
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::NoopQueryParser;
use pgwire::api::{ClientInfo, PgWireConnectionState, PgWireHandlerFactory, Type, METADATA_USER};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::ErrorResponse;
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::db::SqlResult;

pub struct DuckdbProcessor {
    query_parser: Arc<NoopQueryParser>,
    parameters: Arc<RsduckServerParameterProvider>,
}

impl DuckdbProcessor {
    pub fn new() -> Self {
        Self {
            query_parser: Arc::new(NoopQueryParser::new()),
            parameters: Arc::new(RsduckServerParameterProvider),
        }
    }
}

#[derive(Debug)]
struct RsduckServerParameterProvider;

impl ServerParameterProvider for RsduckServerParameterProvider {
    fn server_parameters<C>(&self, _client: &C) -> Option<HashMap<String, String>>
    where
        C: ClientInfo,
    {
        let mut params = HashMap::new();
        params.insert("server_version".to_string(), "14.0".to_string());
        params.insert("server_encoding".to_string(), "UTF8".to_string());
        params.insert("client_encoding".to_string(), "UTF8".to_string());
        params.insert("DateStyle".to_string(), "ISO, MDY".to_string());
        params.insert("integer_datetimes".to_string(), "on".to_string());
        Some(params)
    }
}

fn query_to_response<'a>(
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
) -> PgWireResult<Response<'a>> {
    let schema = Arc::new(
        columns
            .into_iter()
            .map(|c| FieldInfo::new(c, None, None, Type::TEXT, FieldFormat::Text))
            .collect::<Vec<_>>(),
    );

    let stream = stream::iter(rows.into_iter().map({
        let schema = schema.clone();
        move |row| {
            let mut encoder = DataRowEncoder::new(schema.clone());
            for cell in row {
                encoder.encode_field(&cell)?;
            }
            encoder.finish()
        }
    }));

    Ok(Response::Query(QueryResponse::new(schema, stream)))
}

fn exec_to_response<'a>(command: &str, affected_rows: usize) -> Response<'a> {
    Response::Execution(Tag::new(command).with_rows(affected_rows))
}

async fn execute_pg_sql(sql: String, current_user: &str) -> Result<SqlResult, String> {
    if let Some(result) = crate::pg_compat::compat_result(&sql, current_user) {
        return Ok(result);
    }
    if let Some(rewritten_sql) = crate::pg_compat::rewrite_sql(&sql) {
        return crate::db::execute_sql(rewritten_sql).await;
    }

    crate::catalog::guard_external_sql(&sql)?;
    crate::catalog::reject_unhandled_catalog_projection(&sql)?;
    crate::db::execute_sql_as(current_user.to_string(), sql).await
}

#[async_trait]
impl StartupHandler for DuckdbProcessor {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                save_startup_parameters_to_metadata(client, startup);
                client.set_state(PgWireConnectionState::AuthenticationInProgress);
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(password_message) => {
                let password = password_message.into_password()?.password;
                let username = client
                    .metadata()
                    .get(METADATA_USER)
                    .cloned()
                    .unwrap_or_default();
                if username.is_empty() {
                    send_auth_error(client, "Missing user in startup packet").await?;
                    return Ok(());
                }

                match crate::db::authenticate_user(username, password).await {
                    Ok(()) => {
                        finish_authentication(client, self.parameters.as_ref()).await?;
                    }
                    Err(_) => {
                        send_auth_error(client, "Password authentication failed").await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

async fn send_auth_error<C>(client: &mut C, message: &str) -> PgWireResult<()>
where
    C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
    C::Error: Debug,
    PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
{
    let error_info = ErrorInfo::new(
        "FATAL".to_string(),
        "28P01".to_string(),
        message.to_string(),
    );
    client
        .feed(PgWireBackendMessage::ErrorResponse(ErrorResponse::from(
            error_info,
        )))
        .await?;
    client.close().await?;
    Ok(())
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
            return Ok(vec![Response::EmptyQuery]);
        }

        let current_user = client
            .metadata()
            .get(METADATA_USER)
            .map(|value| value.as_str())
            .unwrap_or("admin");
        match execute_pg_sql(sql, current_user).await.map_err(|e| {
            PgWireError::ApiError(Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))
        })? {
            SqlResult::Query { columns, rows } => Ok(vec![query_to_response(columns, rows)?]),
            SqlResult::Execute {
                command,
                affected_rows,
            } => Ok(vec![exec_to_response(&command, affected_rows)]),
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

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        _statement: &pgwire::api::stmt::StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        Ok(DescribeStatementResponse::new(vec![], vec![]))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        _portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        Ok(DescribePortalResponse::new(vec![]))
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

        let current_user = client
            .metadata()
            .get(METADATA_USER)
            .map(|value| value.as_str())
            .unwrap_or("admin");
        match execute_pg_sql(sql, current_user).await.map_err(|e| {
            PgWireError::ApiError(Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))
        })? {
            SqlResult::Query { columns, rows } => query_to_response(columns, rows),
            SqlResult::Execute {
                command,
                affected_rows,
            } => Ok(exec_to_response(&command, affected_rows)),
        }
    }
}

struct DuckdbHandlerFactory {
    handler: Arc<DuckdbProcessor>,
}

impl PgWireHandlerFactory for DuckdbHandlerFactory {
    type StartupHandler = DuckdbProcessor;
    type SimpleQueryHandler = DuckdbProcessor;
    type ExtendedQueryHandler = DuckdbProcessor;
    type CopyHandler = NoopCopyHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        self.handler.clone()
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        Arc::new(NoopCopyHandler)
    }
}

pub async fn start_pg_server(bind: &str) {
    let listener = TcpListener::bind(bind)
        .await
        .expect("bind pg server failed");
    info!("PG wire protocol listening on {}", bind);

    let factory = Arc::new(DuckdbHandlerFactory {
        handler: Arc::new(DuckdbProcessor::new()),
    });

    loop {
        let (socket, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!("PG accept error: {}", e);
                continue;
            }
        };

        info!("PG client connected: {}", addr);
        let handler = factory.clone();
        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, None, handler).await {
                error!("PG session error from {}: {}", addr, e);
            }
            info!("PG client disconnected: {}", addr);
        });
    }
}

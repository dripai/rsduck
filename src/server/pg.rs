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
use pgwire::api::{
    ClientInfo, PgWireConnectionState, PgWireHandlerFactory, Type, METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::ErrorResponse;
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

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
    crate::db::execute_sql_as(current_user.to_string(), sql).await
}

fn metadata_value<'a, C>(client: &'a C, key: &str) -> &'a str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(key)
        .map(|value| value.as_str())
        .unwrap_or("")
}

fn session_user<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(METADATA_USER)
        .map(|value| value.as_str())
        .unwrap_or("admin")
}

fn session_database<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    metadata_value(client, METADATA_DATABASE)
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
                debug!(
                    target: "rsduck::pg",
                    user = %metadata_value(client, METADATA_USER),
                    database = %session_database(client),
                    application_name = %metadata_value(client, "application_name"),
                    "PG startup packet"
                );
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
                    debug!(target: "rsduck::pg", "PG auth rejected: missing user");
                    send_auth_error(client, "Missing user in startup packet").await?;
                    return Ok(());
                }

                let auth_user = username.clone();
                debug!(
                    target: "rsduck::pg",
                    user = %auth_user,
                    database = %session_database(client),
                    "PG auth attempt"
                );
                match crate::db::authenticate_user(username, password).await {
                    Ok(()) => {
                        debug!(
                            target: "rsduck::pg",
                            user = %auth_user,
                            database = %session_database(client),
                            "PG auth accepted"
                        );
                        finish_authentication(client, self.parameters.as_ref()).await?;
                    }
                    Err(_) => {
                        debug!(
                            target: "rsduck::pg",
                            user = %auth_user,
                            database = %session_database(client),
                            "PG auth rejected"
                        );
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
        match execute_pg_sql(sql.clone(), current_user).await {
            Ok(SqlResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                Ok(vec![query_to_response(columns, rows)?])
            }
            Ok(SqlResult::Execute {
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
                Err(PgWireError::ApiError(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e,
                ))))
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

    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        statement: &pgwire::api::stmt::StoredStatement<Self::Statement>,
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
        Ok(DescribeStatementResponse::new(vec![], vec![]))
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
        match execute_pg_sql(sql.clone(), current_user).await {
            Ok(SqlResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                query_to_response(columns, rows)
            }
            Ok(SqlResult::Execute {
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
                Err(PgWireError::ApiError(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e,
                ))))
            }
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
        debug!(target: "rsduck::pg", peer = %addr, "PG connection accepted");
        let handler = factory.clone();
        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, None, handler).await {
                error!("PG session error from {}: {}", addr, e);
            }
            debug!(target: "rsduck::pg", peer = %addr, "PG connection closed");
            info!("PG client disconnected: {}", addr);
        });
    }
}

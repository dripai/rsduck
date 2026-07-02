use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Sink};
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::NoopQueryParser;
use pgwire::api::{ClientInfo, PgWireHandlerFactory, Type};
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::db::SqlResult;

pub struct DuckdbProcessor {
    query_parser: Arc<NoopQueryParser>,
}

impl DuckdbProcessor {
    pub fn new() -> Self {
        Self {
            query_parser: Arc::new(NoopQueryParser::new()),
        }
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

impl NoopStartupHandler for DuckdbProcessor {}

#[async_trait]
impl SimpleQueryHandler for DuckdbProcessor {
    async fn do_query<'a, C>(
        &self,
        _client: &mut C,
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

        match crate::db::execute_sql(sql).await.map_err(|e| {
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
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let sql = portal.statement.statement.to_string();

        match crate::db::execute_sql(sql).await.map_err(|e| {
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

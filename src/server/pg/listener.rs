use std::sync::Arc;

use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::PgWireHandlerFactory;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::db::DbHandle;

use super::handler::DuckdbProcessor;

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

pub async fn start_pg_server(bind: &str, db: DbHandle) {
    let listener = TcpListener::bind(bind)
        .await
        .expect("bind pg server failed");
    serve_pg_listener(listener, db).await;
}

pub(super) async fn serve_pg_listener(listener: TcpListener, db: DbHandle) {
    let bind = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    info!("PG wire protocol listening on {}", bind);

    let factory = Arc::new(DuckdbHandlerFactory {
        handler: Arc::new(DuckdbProcessor::new(db)),
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

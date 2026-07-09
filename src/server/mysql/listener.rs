use std::sync::atomic::{AtomicU32, Ordering};

use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

use crate::db::DbHandle;

use super::auth::authenticate_mysql_client;
use super::codec::{read_packet, write_packet};
use super::command::command_loop;
use super::handshake::{build_initial_handshake, new_nonce, parse_handshake_response};
use super::session::MySqlSession;

static NEXT_CONNECTION_ID: AtomicU32 = AtomicU32::new(1000);

pub async fn start_mysql_server(bind: &str, db: DbHandle) {
    let listener = TcpListener::bind(bind)
        .await
        .expect("bind mysql server failed");
    serve_mysql_listener(listener, db).await;
}

pub(super) async fn serve_mysql_listener(listener: TcpListener, db: DbHandle) {
    let bind = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    info!("MySQL wire protocol listening on {}", bind);

    loop {
        let (socket, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!("MySQL accept error: {}", e);
                continue;
            }
        };

        info!("MySQL client connected: {}", addr);
        debug!(target: "rsduck::mysql", peer = %addr, "MySQL connection accepted");
        let session_db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_mysql_socket(socket, session_db).await {
                debug!(target: "rsduck::mysql", peer = %addr, error = %e, "MySQL session ended");
            }
            info!("MySQL client disconnected: {}", addr);
        });
    }
}

async fn handle_mysql_socket(mut stream: TcpStream, db: DbHandle) -> Result<(), String> {
    let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let nonce = new_nonce();
    let mut sequence = 0_u8;
    write_packet(
        &mut stream,
        &mut sequence,
        &build_initial_handshake(connection_id, &nonce),
    )
    .await?;

    let response_packet = read_packet(&mut stream).await?;
    let response = parse_handshake_response(&response_packet.payload).map_err(|e| {
        format!(
            "parse MySQL handshake response failed at sequence {}: {e}",
            response_packet.sequence
        )
    })?;
    let mut auth_sequence = response_packet.sequence.wrapping_add(1);
    let username =
        authenticate_mysql_client(&mut stream, &mut auth_sequence, &db, nonce, &response).await?;

    let session = MySqlSession::new(
        username,
        response.database,
        response.capabilities,
        response.attrs,
    );
    command_loop(&mut stream, db, session).await
}

use tokio::net::TcpStream;
use tracing::debug;

use crate::auth::{AuthCredential, AuthProtocol, AuthRequest, MYSQL_DEFAULT_AUTH_PLUGIN};
use crate::db::DbHandle;

use super::codec::{err_packet, ok_packet, write_packet};
use super::handshake::HandshakeResponse;
use super::types::CLIENT_PLUGIN_AUTH;

pub(super) async fn authenticate_mysql_client(
    stream: &mut TcpStream,
    sequence: &mut u8,
    db: &DbHandle,
    nonce: Vec<u8>,
    response: &HandshakeResponse,
) -> Result<String, String> {
    if response.capabilities & CLIENT_PLUGIN_AUTH == 0 {
        write_packet(
            stream,
            sequence,
            &err_packet(1251, "08004", "CLIENT_PLUGIN_AUTH is required"),
        )
        .await?;
        return Err("MySQL client does not support plugin auth".into());
    }
    if response.auth_plugin.as_deref() != Some(MYSQL_DEFAULT_AUTH_PLUGIN) {
        write_packet(
            stream,
            sequence,
            &err_packet(
                1251,
                "08004",
                "rsduck MySQL protocol requires caching_sha2_password",
            ),
        )
        .await?;
        return Err("MySQL client did not select caching_sha2_password".into());
    }

    let request = AuthRequest {
        protocol: AuthProtocol::MySqlWire,
        username: response.username.clone(),
        credential: AuthCredential::MySqlCachingSha2Password {
            nonce,
            response: response.auth_response.clone(),
        },
    };

    match db.authenticate(request).await {
        Ok(principal) => {
            debug!(
                target: "rsduck::mysql",
                user = %principal.username,
                "MySQL auth accepted"
            );
            write_packet(stream, sequence, &[0x01, 0x03]).await?;
            write_packet(stream, sequence, &ok_packet()).await?;
            Ok(principal.username)
        }
        Err(_) => {
            write_packet(
                stream,
                sequence,
                &err_packet(1045, "28000", "Access denied for user"),
            )
            .await?;
            Err("MySQL authentication failed".into())
        }
    }
}

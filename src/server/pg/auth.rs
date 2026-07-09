use std::collections::HashMap;
use std::fmt::Debug;

use async_trait::async_trait;
use futures::{Sink, SinkExt};
use pgwire::api::auth::{
    finish_authentication, save_startup_parameters_to_metadata, ServerParameterProvider,
    StartupHandler,
};
use pgwire::api::{ClientInfo, PgWireConnectionState, METADATA_USER};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::ErrorResponse;
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use tracing::debug;

use super::handler::DuckdbProcessor;
use super::session::{metadata_value, session_database};

#[derive(Debug)]
pub(super) struct RsduckServerParameterProvider;

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
                match self.db.authenticate_user(username, password).await {
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

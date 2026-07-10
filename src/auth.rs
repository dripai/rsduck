use duckdb::Connection;

pub const MYSQL_DEFAULT_AUTH_PLUGIN: &str = "caching_sha2_password";
pub const MYSQL_LEGACY_NATIVE_AUTH_PLUGIN: &str = "mysql_native_password";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AuthProtocol {
    WebApi,
    MySqlWire,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AuthCredential {
    CleartextPassword(String),
    MySqlNativePassword { nonce: Vec<u8>, response: Vec<u8> },
    MySqlCachingSha2Password { nonce: Vec<u8>, response: Vec<u8> },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AuthRequest {
    pub protocol: AuthProtocol,
    pub username: String,
    pub credential: AuthCredential,
}

impl AuthRequest {
    pub fn cleartext(
        protocol: AuthProtocol,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            username: username.into(),
            credential: AuthCredential::CleartextPassword(password.into()),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AuthenticatedPrincipal {
    pub user_id: i64,
    pub username: String,
}

pub trait BlockingAuthenticator {
    fn authenticate(
        &self,
        conn: &Connection,
        request: &AuthRequest,
    ) -> Result<AuthenticatedPrincipal, String>;
}

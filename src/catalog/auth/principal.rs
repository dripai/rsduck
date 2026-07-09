use super::*;
use crate::auth::{
    AuthCredential, AuthRequest, AuthenticatedPrincipal, BlockingAuthenticator,
    MYSQL_DEFAULT_AUTH_PLUGIN, MYSQL_LEGACY_NATIVE_AUTH_PLUGIN,
};

pub(crate) struct CatalogAuthenticator;

impl BlockingAuthenticator for CatalogAuthenticator {
    fn authenticate(
        &self,
        conn: &Connection,
        request: &AuthRequest,
    ) -> Result<AuthenticatedPrincipal, String> {
        match (request.protocol, &request.credential) {
            (
                crate::auth::AuthProtocol::WebApi | crate::auth::AuthProtocol::PgWire,
                AuthCredential::CleartextPassword(password),
            ) => authenticate_cleartext_user(conn, &request.username, password),
            (crate::auth::AuthProtocol::MySqlWire, AuthCredential::MySqlNativePassword { .. }) => {
                warn_auth_failure(
                    &request.username,
                    "unsupported_auth_mechanism",
                    MYSQL_LEGACY_NATIVE_AUTH_PLUGIN,
                );
                Err(AUTH_FAILED.into())
            }
            (
                crate::auth::AuthProtocol::MySqlWire,
                AuthCredential::MySqlCachingSha2Password { nonce, response },
            ) => authenticate_mysql_caching_sha2_user(conn, &request.username, nonce, response),
            _ => {
                warn_auth_failure(&request.username, "protocol_credential_mismatch", "unknown");
                Err(AUTH_FAILED.into())
            }
        }
    }
}

#[cfg(test)]
pub fn authenticate_user(conn: &Connection, username: &str, password: &str) -> Result<i64, String> {
    authenticate_cleartext_user(conn, username, password).map(|principal| principal.user_id)
}

fn authenticate_cleartext_user(
    conn: &Connection,
    username: &str,
    password: &str,
) -> Result<AuthenticatedPrincipal, String> {
    let mut stmt = conn
        .prepare(
            "SELECT user_id, password_hash, password_algo, status \
             FROM rsduck_catalog.rs_user \
             WHERE username = ?",
        )
        .map_err(|e| format!("prepare user authentication failed: {e}"))?;
    let mut rows = stmt
        .query([username])
        .map_err(|e| format!("query user authentication failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read user authentication failed: {e}"))?
    else {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = "unknown_user"
        );
        return Err(AUTH_FAILED.into());
    };

    let user_id: i64 = row
        .get(0)
        .map_err(|e| format!("read authenticated user id failed: {e}"))?;
    let password_hash: String = row
        .get(1)
        .map_err(|e| format!("read password hash failed: {e}"))?;
    let password_algo: String = row
        .get(2)
        .map_err(|e| format!("read password algo failed: {e}"))?;
    let status: String = row
        .get(3)
        .map_err(|e| format!("read user status failed: {e}"))?;

    if status != "active" {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = status.as_str()
        );
        return Err(AUTH_FAILED.into());
    }
    if password_algo != "argon2id" {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = "unsupported_password_algorithm",
            password_algo = password_algo.as_str()
        );
        return Err(AUTH_FAILED.into());
    }
    if !verify_password(password, &password_hash) {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = "password_mismatch"
        );
        return Err(AUTH_FAILED.into());
    }

    info!(
        target: "rsduck_audit",
        event = "login_success",
        username = username,
        user_id = user_id
    );
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_user \
             SET last_login_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
             WHERE user_id = {user_id}"
        ),
        [],
    )
    .map_err(|e| format!("update last login timestamp failed: {e}"))?;
    Ok(AuthenticatedPrincipal {
        user_id,
        username: username.to_string(),
    })
}

fn authenticate_mysql_caching_sha2_user(
    conn: &Connection,
    username: &str,
    nonce: &[u8],
    response: &[u8],
) -> Result<AuthenticatedPrincipal, String> {
    let row = read_auth_user(conn, username)?;
    if row.status != "active" {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = row.status.as_str()
        );
        return Err(AUTH_FAILED.into());
    }
    if row.mysql_auth_plugin != MYSQL_DEFAULT_AUTH_PLUGIN || row.mysql_auth_string.is_empty() {
        warn_auth_failure(
            username,
            "mysql_verifier_not_available",
            MYSQL_DEFAULT_AUTH_PLUGIN,
        );
        return Err(AUTH_FAILED.into());
    }
    if !verify_mysql_caching_sha2_password(nonce, response, &row.mysql_auth_string) {
        warn_auth_failure(username, "password_mismatch", MYSQL_DEFAULT_AUTH_PLUGIN);
        return Err(AUTH_FAILED.into());
    }
    finish_successful_auth(conn, username, row.user_id)
}

struct AuthUserRow {
    user_id: i64,
    mysql_auth_plugin: String,
    mysql_auth_string: String,
    status: String,
}

fn read_auth_user(conn: &Connection, username: &str) -> Result<AuthUserRow, String> {
    let mut stmt = conn
        .prepare(
            "SELECT user_id, mysql_auth_plugin, mysql_auth_string, status \
             FROM rsduck_catalog.rs_user \
             WHERE username = ?",
        )
        .map_err(|e| format!("prepare user authentication failed: {e}"))?;
    let mut rows = stmt
        .query([username])
        .map_err(|e| format!("query user authentication failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read user authentication failed: {e}"))?
    else {
        warn!(
            target: "rsduck_audit",
            event = "login_failure",
            username = username,
            reason = "unknown_user"
        );
        return Err(AUTH_FAILED.into());
    };

    Ok(AuthUserRow {
        user_id: row
            .get(0)
            .map_err(|e| format!("read authenticated user id failed: {e}"))?,
        mysql_auth_plugin: row
            .get(1)
            .map_err(|e| format!("read mysql auth plugin failed: {e}"))?,
        mysql_auth_string: row
            .get(2)
            .map_err(|e| format!("read mysql auth string failed: {e}"))?,
        status: row
            .get(3)
            .map_err(|e| format!("read user status failed: {e}"))?,
    })
}

fn finish_successful_auth(
    conn: &Connection,
    username: &str,
    user_id: i64,
) -> Result<AuthenticatedPrincipal, String> {
    info!(
        target: "rsduck_audit",
        event = "login_success",
        username = username,
        user_id = user_id
    );
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_user \
             SET last_login_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
             WHERE user_id = {user_id}"
        ),
        [],
    )
    .map_err(|e| format!("update last login timestamp failed: {e}"))?;
    Ok(AuthenticatedPrincipal {
        user_id,
        username: username.to_string(),
    })
}

fn warn_auth_failure(username: &str, reason: &str, mechanism: &str) {
    warn!(
        target: "rsduck_audit",
        event = "login_failure",
        username = username,
        reason = reason,
        mechanism = mechanism
    );
}

pub(in crate::catalog) fn principal_for_username(
    conn: &Connection,
    username: &str,
) -> Result<SessionPrincipal, String> {
    let (user_id, status): (i64, String) = conn
        .query_row(
            &format!(
                "SELECT user_id, status FROM rsduck_catalog.rs_user WHERE username = '{}'",
                sql_string(username)
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| format!("unknown user: {username}"))?;
    if status != "active" {
        return Err(format!("user is not active: {username}"));
    }

    let mut stmt = conn
        .prepare(&format!(
            "SELECT r.role_name \
                 FROM rsduck_catalog.rs_user_role ur \
                 JOIN rsduck_catalog.rs_role r ON r.role_id = ur.role_id \
                 WHERE ur.user_id = {user_id} \
                 ORDER BY r.role_name"
        ))
        .map_err(|e| format!("prepare principal roles failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query principal roles failed: {e}"))?;
    let mut roles = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read principal roles failed: {e}"))?
    {
        roles.push(
            row.get(0)
                .map_err(|e| format!("read principal role name failed: {e}"))?,
        );
    }

    Ok(SessionPrincipal {
        user_id,
        username: username.to_string(),
        roles,
    })
}

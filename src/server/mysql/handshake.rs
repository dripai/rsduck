use std::collections::HashMap;

use rand_core::{OsRng, RngCore};

use crate::auth::MYSQL_DEFAULT_AUTH_PLUGIN;

use super::codec::{
    get_lenenc_bytes, get_lenenc_int, get_null_str, put_null_str, put_u16_le, put_u32_le, take,
};
use super::types::{
    server_capabilities, CLIENT_CONNECT_ATTRS, CLIENT_CONNECT_WITH_DB, CLIENT_PLUGIN_AUTH,
    CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
    SERVER_STATUS_AUTOCOMMIT,
};

pub(super) const MYSQL_SERVER_VERSION: &str = "8.0.34-rsduck";
pub(super) const MYSQL_AUTH_NONCE_LEN: usize = 20;

#[derive(Debug, Clone)]
pub(super) struct HandshakeResponse {
    pub capabilities: u32,
    pub username: String,
    pub database: Option<String>,
    pub auth_response: Vec<u8>,
    pub auth_plugin: Option<String>,
    pub attrs: HashMap<String, String>,
}

pub(super) fn new_nonce() -> Vec<u8> {
    let mut nonce = vec![0_u8; MYSQL_AUTH_NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

pub(super) fn build_initial_handshake(connection_id: u32, nonce: &[u8]) -> Vec<u8> {
    let capabilities = server_capabilities();
    let mut out = Vec::new();
    out.push(0x0a);
    put_null_str(&mut out, MYSQL_SERVER_VERSION);
    put_u32_le(&mut out, connection_id);
    out.extend_from_slice(&nonce[..8]);
    out.push(0);
    put_u16_le(&mut out, (capabilities & 0xffff) as u16);
    out.push(45);
    put_u16_le(&mut out, SERVER_STATUS_AUTOCOMMIT);
    put_u16_le(&mut out, (capabilities >> 16) as u16);
    out.push((nonce.len() + 1) as u8);
    out.extend_from_slice(&[0_u8; 10]);
    out.extend_from_slice(&nonce[8..]);
    out.push(0);
    put_null_str(&mut out, MYSQL_DEFAULT_AUTH_PLUGIN);
    out
}

pub(super) fn parse_handshake_response(payload: &[u8]) -> Result<HandshakeResponse, String> {
    let mut idx = 0;
    let capabilities = super::codec::get_u32_le(payload, &mut idx)?;
    if capabilities & CLIENT_PROTOCOL_41 == 0 {
        return Err("MySQL client does not support protocol 4.1".into());
    }
    let _max_packet_size = super::codec::get_u32_le(payload, &mut idx)?;
    let _charset = *take(payload, &mut idx, 1)?
        .first()
        .ok_or_else(|| "missing charset".to_string())?;
    take(payload, &mut idx, 23)?;

    let username = get_null_str(payload, &mut idx)?;
    let auth_response = if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        get_lenenc_bytes(payload, &mut idx)?
    } else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let len = *take(payload, &mut idx, 1)?
            .first()
            .ok_or_else(|| "missing auth response length".to_string())? as usize;
        take(payload, &mut idx, len)?.to_vec()
    } else {
        return Err("MySQL client does not support secure auth response".into());
    };

    let database = if capabilities & CLIENT_CONNECT_WITH_DB != 0 && idx < payload.len() {
        Some(get_null_str(payload, &mut idx)?).filter(|value| !value.is_empty())
    } else {
        None
    };
    let auth_plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 && idx < payload.len() {
        Some(get_null_str(payload, &mut idx)?).filter(|value| !value.is_empty())
    } else {
        None
    };
    let attrs = if capabilities & CLIENT_CONNECT_ATTRS != 0 && idx < payload.len() {
        parse_connect_attrs(payload, &mut idx)?
    } else {
        HashMap::new()
    };

    Ok(HandshakeResponse {
        capabilities,
        username,
        database,
        auth_response,
        auth_plugin,
        attrs,
    })
}

fn parse_connect_attrs(payload: &[u8], idx: &mut usize) -> Result<HashMap<String, String>, String> {
    let total = get_lenenc_int(payload, idx)? as usize;
    let end = idx
        .checked_add(total)
        .ok_or_else(|| "connection attrs length overflow".to_string())?;
    if end > payload.len() {
        return Err("truncated connection attrs".into());
    }

    let mut attrs = HashMap::new();
    while *idx < end {
        let key = String::from_utf8(get_lenenc_bytes(payload, idx)?)
            .map_err(|e| format!("invalid connection attr key: {e}"))?;
        let value = String::from_utf8(get_lenenc_bytes(payload, idx)?)
            .map_err(|e| format!("invalid connection attr value: {e}"))?;
        attrs.insert(key, value);
    }
    Ok(attrs)
}

use super::*;
use sha2::{Digest, Sha256};

pub const MYSQL_CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("hash password failed: {e}"))
}

pub fn verify_password(password: &str, encoded_hash: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

pub fn mysql_caching_sha2_verifier(password: &str) -> String {
    hex_encode(&mysql_stage2_hash(password))
}

pub fn verify_mysql_caching_sha2_password(
    nonce: &[u8],
    response: &[u8],
    verifier_hex: &str,
) -> bool {
    let Some(verifier) = hex_decode(verifier_hex) else {
        return false;
    };
    if verifier.len() != 32 || response.len() != 32 {
        return false;
    }

    let mut digest = Sha256::new();
    digest.update(&verifier);
    digest.update(nonce);
    let scramble = digest.finalize();

    let stage1 = response
        .iter()
        .zip(scramble.iter())
        .map(|(left, right)| left ^ right)
        .collect::<Vec<_>>();
    let candidate = Sha256::digest(&stage1);
    candidate.as_slice() == verifier.as_slice()
}

fn mysql_stage2_hash(password: &str) -> [u8; 32] {
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    stage2.into()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let raw = value.as_bytes();
    for idx in (0..raw.len()).step_by(2) {
        let hi = hex_digit(raw[idx])?;
        let lo = hex_digit(raw[idx + 1])?;
        bytes.push((hi << 4) | lo);
    }
    Some(bytes)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

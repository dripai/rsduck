use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_PACKET_LEN: usize = 0x00ff_ffff;

#[derive(Debug, Clone)]
pub(super) struct Packet {
    pub sequence: u8,
    pub payload: Vec<u8>,
}

pub(super) async fn read_packet(stream: &mut TcpStream) -> Result<Packet, String> {
    let mut payload = Vec::new();
    let sequence = loop {
        let mut header = [0_u8; 4];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|e| format!("read MySQL packet header failed: {e}"))?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        let sequence = header[3];
        let mut chunk = vec![0_u8; len];
        stream
            .read_exact(&mut chunk)
            .await
            .map_err(|e| format!("read MySQL packet payload failed: {e}"))?;
        payload.extend_from_slice(&chunk);
        if len < MAX_PACKET_LEN {
            break sequence;
        }
    };
    Ok(Packet { sequence, payload })
}

pub(super) async fn write_packet(
    stream: &mut TcpStream,
    sequence: &mut u8,
    payload: &[u8],
) -> Result<(), String> {
    let mut offset = 0;
    loop {
        let remaining = payload.len() - offset;
        let len = remaining.min(MAX_PACKET_LEN);
        let mut header = [0_u8; 4];
        header[0] = (len & 0xff) as u8;
        header[1] = ((len >> 8) & 0xff) as u8;
        header[2] = ((len >> 16) & 0xff) as u8;
        header[3] = *sequence;
        stream
            .write_all(&header)
            .await
            .map_err(|e| format!("write MySQL packet header failed: {e}"))?;
        stream
            .write_all(&payload[offset..offset + len])
            .await
            .map_err(|e| format!("write MySQL packet payload failed: {e}"))?;
        *sequence = sequence.wrapping_add(1);
        offset += len;
        if len < MAX_PACKET_LEN {
            break;
        }
    }
    Ok(())
}

pub(super) fn put_u16_le(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn put_u24_le(out: &mut Vec<u8>, value: u32) {
    out.push((value & 0xff) as u8);
    out.push(((value >> 8) & 0xff) as u8);
    out.push(((value >> 16) & 0xff) as u8);
}

pub(super) fn put_u32_le(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn put_u64_le(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn put_lenenc_int(out: &mut Vec<u8>, value: u64) {
    if value < 251 {
        out.push(value as u8);
    } else if value <= 0xffff {
        out.push(0xfc);
        put_u16_le(out, value as u16);
    } else if value <= 0x00ff_ffff {
        out.push(0xfd);
        put_u24_le(out, value as u32);
    } else {
        out.push(0xfe);
        put_u64_le(out, value);
    }
}

pub(super) fn put_lenenc_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_lenenc_int(out, value.len() as u64);
    out.extend_from_slice(value);
}

pub(super) fn put_lenenc_str(out: &mut Vec<u8>, value: &str) {
    put_lenenc_bytes(out, value.as_bytes());
}

pub(super) fn put_null_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

pub(super) fn get_u16_le(input: &[u8], idx: &mut usize) -> Result<u16, String> {
    let bytes = take(input, idx, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(super) fn get_u32_le(input: &[u8], idx: &mut usize) -> Result<u32, String> {
    let bytes = take(input, idx, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(super) fn get_u64_le(input: &[u8], idx: &mut usize) -> Result<u64, String> {
    let bytes = take(input, idx, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

pub(super) fn get_lenenc_int(input: &[u8], idx: &mut usize) -> Result<u64, String> {
    let first = *take(input, idx, 1)?
        .first()
        .ok_or_else(|| "missing length encoded integer".to_string())?;
    match first {
        0xfc => Ok(get_u16_le(input, idx)? as u64),
        0xfd => {
            let bytes = take(input, idx, 3)?;
            Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as u64)
        }
        0xfe => get_u64_le(input, idx),
        0xff => Err("invalid length encoded integer marker 0xff".into()),
        value => Ok(value as u64),
    }
}

pub(super) fn get_lenenc_bytes(input: &[u8], idx: &mut usize) -> Result<Vec<u8>, String> {
    let len = get_lenenc_int(input, idx)? as usize;
    Ok(take(input, idx, len)?.to_vec())
}

pub(super) fn get_null_str(input: &[u8], idx: &mut usize) -> Result<String, String> {
    let start = *idx;
    while *idx < input.len() && input[*idx] != 0 {
        *idx += 1;
    }
    if *idx >= input.len() {
        return Err("missing null terminator".into());
    }
    let value = std::str::from_utf8(&input[start..*idx])
        .map_err(|e| format!("invalid UTF-8 string: {e}"))?
        .to_string();
    *idx += 1;
    Ok(value)
}

pub(super) fn take<'a>(input: &'a [u8], idx: &mut usize, len: usize) -> Result<&'a [u8], String> {
    if input.len().saturating_sub(*idx) < len {
        return Err("truncated MySQL packet".into());
    }
    let out = &input[*idx..*idx + len];
    *idx += len;
    Ok(out)
}

pub(super) fn ok_packet() -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x00);
    put_lenenc_int(&mut out, 0);
    put_lenenc_int(&mut out, 0);
    put_u16_le(&mut out, super::types::SERVER_STATUS_AUTOCOMMIT);
    put_u16_le(&mut out, 0);
    out
}

pub(super) fn ok_packet_with_rows(affected_rows: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x00);
    put_lenenc_int(&mut out, affected_rows as u64);
    put_lenenc_int(&mut out, 0);
    put_u16_le(&mut out, super::types::SERVER_STATUS_AUTOCOMMIT);
    put_u16_le(&mut out, 0);
    out
}

pub(super) fn eof_packet() -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(0xfe);
    put_u16_le(&mut out, 0);
    put_u16_le(&mut out, super::types::SERVER_STATUS_AUTOCOMMIT);
    out
}

pub(super) fn err_packet(code: u16, state: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0xff);
    put_u16_le(&mut out, code);
    out.push(b'#');
    let mut sql_state = [b'H', b'Y', b'0', b'0', b'0'];
    for (idx, byte) in state.as_bytes().iter().take(5).enumerate() {
        sql_state[idx] = *byte;
    }
    out.extend_from_slice(&sql_state);
    out.extend_from_slice(message.as_bytes());
    out
}

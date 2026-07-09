use crate::db::SqlType;

pub(super) const COM_QUIT: u8 = 0x01;
pub(super) const COM_INIT_DB: u8 = 0x02;
pub(super) const COM_QUERY: u8 = 0x03;
pub(super) const COM_PING: u8 = 0x0e;
pub(super) const COM_STMT_PREPARE: u8 = 0x16;
pub(super) const COM_STMT_EXECUTE: u8 = 0x17;
pub(super) const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
pub(super) const COM_STMT_CLOSE: u8 = 0x19;
pub(super) const COM_STMT_RESET: u8 = 0x1a;

pub(super) const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
pub(super) const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
pub(super) const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub(super) const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub(super) const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub(super) const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub(super) const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
pub(super) const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
pub(super) const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
pub(super) const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
pub(super) const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;

pub(super) const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

pub(super) const MYSQL_TYPE_TINY: u8 = 0x01;
pub(super) const MYSQL_TYPE_SHORT: u8 = 0x02;
pub(super) const MYSQL_TYPE_LONG: u8 = 0x03;
pub(super) const MYSQL_TYPE_FLOAT: u8 = 0x04;
pub(super) const MYSQL_TYPE_DOUBLE: u8 = 0x05;
pub(super) const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
pub(super) const MYSQL_TYPE_LONGLONG: u8 = 0x08;
pub(super) const MYSQL_TYPE_DATE: u8 = 0x0a;
pub(super) const MYSQL_TYPE_TIME: u8 = 0x0b;
pub(super) const MYSQL_TYPE_DATETIME: u8 = 0x0c;
pub(super) const MYSQL_TYPE_JSON: u8 = 0xf5;
pub(super) const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
pub(super) const MYSQL_TYPE_BLOB: u8 = 0xfc;
pub(super) const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
pub(super) const MYSQL_TYPE_STRING: u8 = 0xfe;

pub(super) const MYSQL_CHARSET_BINARY: u16 = 63;
pub(super) const MYSQL_CHARSET_UTF8MB4: u16 = 45;

pub(super) const MYSQL_FLAG_BINARY: u16 = 0x0080;
pub(super) const MYSQL_FLAG_NUM: u16 = 0x8000;

pub(super) fn server_capabilities() -> u32 {
    CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_PROTOCOL_41
        | CLIENT_SECURE_CONNECTION
        | CLIENT_TRANSACTIONS
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
        | CLIENT_PLUGIN_AUTH
        | CLIENT_CONNECT_ATTRS
        | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
}

pub(super) fn mysql_type_for_sql_type(data_type: SqlType) -> (u8, u16, u16) {
    match data_type {
        SqlType::Bool => (MYSQL_TYPE_TINY, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Int2 => (MYSQL_TYPE_SHORT, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Int4 => (MYSQL_TYPE_LONG, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Int8 => (MYSQL_TYPE_LONGLONG, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Float4 => (MYSQL_TYPE_FLOAT, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Float8 => (MYSQL_TYPE_DOUBLE, MYSQL_CHARSET_BINARY, MYSQL_FLAG_NUM),
        SqlType::Numeric => (MYSQL_TYPE_NEWDECIMAL, MYSQL_CHARSET_UTF8MB4, MYSQL_FLAG_NUM),
        SqlType::Date => (MYSQL_TYPE_DATE, MYSQL_CHARSET_BINARY, 0),
        SqlType::Time => (MYSQL_TYPE_TIME, MYSQL_CHARSET_BINARY, 0),
        SqlType::Timestamp => (MYSQL_TYPE_DATETIME, MYSQL_CHARSET_BINARY, 0),
        SqlType::TimestampTz => (MYSQL_TYPE_TIMESTAMP, MYSQL_CHARSET_BINARY, 0),
        SqlType::Bytea => (MYSQL_TYPE_BLOB, MYSQL_CHARSET_BINARY, MYSQL_FLAG_BINARY),
        SqlType::Json => (MYSQL_TYPE_JSON, MYSQL_CHARSET_UTF8MB4, 0),
        SqlType::Uuid | SqlType::Text => (MYSQL_TYPE_VAR_STRING, MYSQL_CHARSET_UTF8MB4, 0),
    }
}

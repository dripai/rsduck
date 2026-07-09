use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Sink, SinkExt};
use pgwire::api::auth::{
    finish_authentication, save_startup_parameters_to_metadata, ServerParameterProvider,
    StartupHandler,
};
use pgwire::api::copy::NoopCopyHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{
    ClientInfo, ClientPortalStore, PgWireConnectionState, PgWireHandlerFactory, Type, DEFAULT_NAME,
    METADATA_DATABASE, METADATA_USER,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::extendedquery::{Parse, ParseComplete};
use pgwire::messages::response::ErrorResponse;
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::db::{SqlColumn, SqlParam, SqlTypedResult};

pub struct DuckdbProcessor {
    query_parser: Arc<NoopQueryParser>,
    parameters: Arc<RsduckServerParameterProvider>,
}

impl DuckdbProcessor {
    pub fn new() -> Self {
        Self {
            query_parser: Arc::new(NoopQueryParser::new()),
            parameters: Arc::new(RsduckServerParameterProvider),
        }
    }
}

#[derive(Debug)]
struct RsduckServerParameterProvider;

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

fn query_to_response<'a>(
    columns: Vec<SqlColumn>,
    rows: Vec<Vec<Option<String>>>,
    format: FieldFormat,
) -> PgWireResult<Response<'a>> {
    let schema = Arc::new(fields_for_columns(columns, format));

    let stream = stream::iter(rows.into_iter().map({
        let schema = schema.clone();
        move |row| {
            let mut encoder = DataRowEncoder::new(schema.clone());
            for (idx, cell) in row.into_iter().enumerate() {
                let Some(field) = schema.get(idx) else {
                    return Err(pg_api_error(format!(
                        "row has more fields than described columns: index {idx}"
                    )));
                };
                encode_pg_cell(
                    &mut encoder,
                    cell.as_deref(),
                    field.datatype(),
                    field.format(),
                )?;
            }
            encoder.finish()
        }
    }));

    Ok(Response::Query(QueryResponse::new(schema, stream)))
}

fn exec_to_response<'a>(command: &str, affected_rows: usize) -> Response<'a> {
    Response::Execution(Tag::new(command).with_rows(affected_rows))
}

async fn execute_pg_sql(
    sql: String,
    current_user: &str,
    params: Vec<SqlParam>,
) -> Result<SqlTypedResult, String> {
    crate::db::execute_typed_sql_with_params_as(current_user.to_string(), sql, params).await
}

async fn describe_pg_sql(
    sql: String,
    current_user: &str,
    params: Vec<SqlParam>,
) -> Result<Vec<SqlColumn>, String> {
    crate::db::describe_sql_with_params_as(current_user.to_string(), sql, params).await
}

fn fields_for_columns(columns: Vec<SqlColumn>, format: FieldFormat) -> Vec<FieldInfo> {
    columns
        .into_iter()
        .map(|c| {
            let pg_type = Type::from_oid(c.pg_type_oid).unwrap_or(Type::TEXT);
            FieldInfo::new(c.name, None, None, pg_type, format)
        })
        .collect()
}

fn encode_pg_cell(
    encoder: &mut DataRowEncoder,
    value: Option<&str>,
    data_type: &Type,
    format: FieldFormat,
) -> PgWireResult<()> {
    let Some(value) = value else {
        return encoder.encode_field_with_type_and_format(&None::<i8>, data_type, format);
    };

    if format == FieldFormat::Text {
        return encoder.encode_field_with_type_and_format(&value, data_type, format);
    }

    match *data_type {
        Type::BOOL => {
            let value = parse_pg_bool(value)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT2 => {
            let value = value
                .parse::<i16>()
                .map_err(|e| pg_api_error(format!("failed to encode int2 value '{value}': {e}")))?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT4 => {
            let value = value
                .parse::<i32>()
                .map_err(|e| pg_api_error(format!("failed to encode int4 value '{value}': {e}")))?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT8 => {
            let value = value
                .parse::<i64>()
                .map_err(|e| pg_api_error(format!("failed to encode int8 value '{value}': {e}")))?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::FLOAT4 => {
            let value = value.parse::<f32>().map_err(|e| {
                pg_api_error(format!("failed to encode float4 value '{value}': {e}"))
            })?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::FLOAT8 => {
            let value = value.parse::<f64>().map_err(|e| {
                pg_api_error(format!("failed to encode float8 value '{value}': {e}"))
            })?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::DATE => {
            let value = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map_err(|e| pg_api_error(format!("failed to encode date value '{value}': {e}")))?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIME => {
            let value = parse_pg_time(value)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIMESTAMP => {
            let value = parse_pg_timestamp(value)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIMESTAMPTZ => {
            let value = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                parse_pg_timestamp(value)?,
                chrono::Utc,
            );
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::BYTEA => {
            let value = parse_pg_bytea(value)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        _ => Err(pg_api_error(format!(
            "unsupported PG binary column type: {}",
            data_type.name()
        ))),
    }
}

fn sql_params_from_portal(portal: &Portal<String>) -> Result<Vec<SqlParam>, String> {
    let mut params = Vec::with_capacity(portal.parameters.len());
    for (idx, param) in portal.parameters.iter().enumerate() {
        let data_type = portal
            .statement
            .parameter_types
            .get(idx)
            .cloned()
            .unwrap_or(Type::UNKNOWN);
        let format = portal.parameter_format.format_for(idx);
        params.push(sql_param_from_pg_value(
            param.as_ref().map(|bytes| bytes.as_ref()),
            &data_type,
            format,
        )?);
    }
    Ok(params)
}

fn sql_param_from_pg_value(
    value: Option<&[u8]>,
    data_type: &Type,
    format: FieldFormat,
) -> Result<SqlParam, String> {
    let Some(value) = value else {
        return Ok(SqlParam::Null);
    };

    if format == FieldFormat::Text {
        let text = std::str::from_utf8(value)
            .map_err(|e| format!("invalid UTF-8 text SQL parameter: {e}"))?;
        return sql_param_from_text(text, data_type);
    }

    match *data_type {
        Type::BOOL => {
            let byte = single_byte(value, data_type)?;
            Ok(SqlParam::Bool(byte != 0))
        }
        Type::INT2 => Ok(SqlParam::Integer(
            i16::from_be_bytes(fixed_bytes(value, data_type)?) as i64,
        )),
        Type::INT4 => Ok(SqlParam::Integer(
            i32::from_be_bytes(fixed_bytes(value, data_type)?) as i64,
        )),
        Type::INT8 => Ok(SqlParam::Integer(i64::from_be_bytes(fixed_bytes(
            value, data_type,
        )?))),
        Type::FLOAT4 => Ok(SqlParam::Float(
            f32::from_bits(u32::from_be_bytes(fixed_bytes(value, data_type)?)) as f64,
        )),
        Type::FLOAT8 => Ok(SqlParam::Float(f64::from_bits(u64::from_be_bytes(
            fixed_bytes(value, data_type)?,
        )))),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            let text = std::str::from_utf8(value)
                .map_err(|e| format!("invalid UTF-8 binary SQL parameter: {e}"))?;
            Ok(SqlParam::Text(text.to_string()))
        }
        Type::DATE => {
            let days = i32::from_be_bytes(fixed_bytes(value, data_type)?);
            let date = pg_epoch_date()
                .checked_add_signed(chrono::Duration::days(days as i64))
                .ok_or_else(|| format!("date SQL parameter is out of range: {days}"))?;
            Ok(SqlParam::Text(date.format("%Y-%m-%d").to_string()))
        }
        Type::TIME => {
            let micros = i64::from_be_bytes(fixed_bytes(value, data_type)?);
            Ok(SqlParam::Text(format_time_micros(micros)?))
        }
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(fixed_bytes(value, data_type)?);
            let timestamp = pg_epoch_datetime()
                .checked_add_signed(chrono::Duration::microseconds(micros))
                .ok_or_else(|| format!("timestamp SQL parameter is out of range: {micros}"))?;
            Ok(SqlParam::Text(
                timestamp.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
            ))
        }
        Type::BYTEA => Ok(SqlParam::Bytes(value.to_vec())),
        Type::NUMERIC => {
            let text = std::str::from_utf8(value)
                .map_err(|_| "binary numeric SQL parameters are not supported".to_string())?;
            sql_param_from_text(text, data_type)
        }
        _ => Err(format!(
            "unsupported PG binary SQL parameter type: {}",
            data_type.name()
        )),
    }
}

fn sql_param_from_text(value: &str, data_type: &Type) -> Result<SqlParam, String> {
    match *data_type {
        Type::BOOL => parse_pg_bool_text(value).map(SqlParam::Bool),
        Type::INT2 | Type::INT4 | Type::INT8 => value
            .parse::<i64>()
            .map(SqlParam::Integer)
            .map_err(|e| format!("invalid integer SQL parameter '{value}': {e}")),
        Type::FLOAT4 | Type::FLOAT8 => value
            .parse::<f64>()
            .map(SqlParam::Float)
            .map_err(|e| format!("invalid float SQL parameter '{value}': {e}")),
        Type::BYTEA => parse_pg_bytea_text(value).map(SqlParam::Bytes),
        _ => Ok(SqlParam::Text(value.to_string())),
    }
}

fn infer_statement_parameter_types(statement: &StoredStatement<String>) -> Vec<Type> {
    infer_parameter_types(&statement.statement, &statement.parameter_types)
}

fn infer_parameter_types(sql: &str, provided_types: &[Type]) -> Vec<Type> {
    let placeholder_count = crate::db::sql_placeholder_count(sql).unwrap_or_default();
    let count = placeholder_count.max(provided_types.len());
    (0..count)
        .map(|idx| {
            let provided = provided_types.get(idx).cloned().unwrap_or(Type::UNKNOWN);
            if provided != Type::UNKNOWN {
                provided
            } else {
                infer_cast_type_for_placeholder(sql, idx + 1).unwrap_or(Type::TEXT)
            }
        })
        .collect()
}

fn dummy_sql_param_for_type(data_type: &Type) -> SqlParam {
    match *data_type {
        Type::BOOL => SqlParam::Bool(false),
        Type::INT2 | Type::INT4 | Type::INT8 => SqlParam::Integer(0),
        Type::FLOAT4 | Type::FLOAT8 => SqlParam::Float(0.0),
        Type::BYTEA => SqlParam::Bytes(Vec::new()),
        _ => SqlParam::Text(String::new()),
    }
}

fn infer_cast_type_for_placeholder(sql: &str, param_number: usize) -> Option<Type> {
    let lower = sql.to_ascii_lowercase();
    let needle = format!("${param_number}");
    for (pos, _) in lower.match_indices(&needle) {
        let after_placeholder = pos + needle.len();
        if lower
            .as_bytes()
            .get(after_placeholder)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Some(type_name) = cast_type_after_placeholder(&lower, after_placeholder) else {
            continue;
        };
        if let Some(data_type) = pg_type_by_name(type_name) {
            return Some(data_type);
        }
    }
    None
}

fn cast_type_after_placeholder(sql: &str, mut idx: usize) -> Option<&str> {
    idx = skip_ascii_space(sql, idx);
    if !sql[idx..].starts_with("::") {
        return None;
    }
    idx = skip_ascii_space(sql, idx + 2);
    let start = idx;
    while sql
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        idx += 1;
    }
    if start == idx {
        None
    } else {
        Some(&sql[start..idx])
    }
}

fn skip_ascii_space(sql: &str, mut idx: usize) -> usize {
    while sql
        .as_bytes()
        .get(idx)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        idx += 1;
    }
    idx
}

fn pg_type_by_name(name: &str) -> Option<Type> {
    match name {
        "bool" | "boolean" => Some(Type::BOOL),
        "int2" | "smallint" => Some(Type::INT2),
        "int4" | "int" | "integer" => Some(Type::INT4),
        "int8" | "bigint" => Some(Type::INT8),
        "float4" | "real" => Some(Type::FLOAT4),
        "float8" | "double" => Some(Type::FLOAT8),
        "text" => Some(Type::TEXT),
        "varchar" => Some(Type::VARCHAR),
        "bpchar" | "char" => Some(Type::BPCHAR),
        "name" => Some(Type::NAME),
        "date" => Some(Type::DATE),
        "time" => Some(Type::TIME),
        "timestamp" => Some(Type::TIMESTAMP),
        "timestamptz" => Some(Type::TIMESTAMPTZ),
        "numeric" | "decimal" => Some(Type::NUMERIC),
        "bytea" | "blob" => Some(Type::BYTEA),
        _ => None,
    }
}

fn parse_pg_bool(value: &str) -> PgWireResult<bool> {
    parse_pg_bool_text(value).map_err(pg_api_error)
}

fn parse_pg_bool_text(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "1" => Ok(true),
        "f" | "false" | "0" => Ok(false),
        _ => Err(format!("invalid boolean value: {value}")),
    }
}

fn parse_pg_time(value: &str) -> PgWireResult<chrono::NaiveTime> {
    chrono::NaiveTime::parse_from_str(value, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .map_err(|e| pg_api_error(format!("failed to encode time value '{value}': {e}")))
}

fn parse_pg_timestamp(value: &str) -> PgWireResult<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S"))
        .map_err(|e| pg_api_error(format!("failed to encode timestamp value '{value}': {e}")))
}

fn parse_pg_bytea(value: &str) -> PgWireResult<Vec<u8>> {
    parse_pg_bytea_text(value).map_err(pg_api_error)
}

fn parse_pg_bytea_text(value: &str) -> Result<Vec<u8>, String> {
    let hex = value.strip_prefix("\\x").unwrap_or(value);
    if hex.len() % 2 != 0 {
        return Err(format!("invalid bytea hex length: {}", hex.len()));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for idx in (0..raw.len()).step_by(2) {
        let hi = hex_digit(raw[idx])?;
        let lo = hex_digit(raw[idx + 1])?;
        bytes.push((hi << 4) | lo);
    }
    Ok(bytes)
}

fn hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", value as char)),
    }
}

fn single_byte(value: &[u8], data_type: &Type) -> Result<u8, String> {
    if value.len() != 1 {
        return Err(format!(
            "invalid binary {} parameter length: expected 1, got {}",
            data_type.name(),
            value.len()
        ));
    }
    Ok(value[0])
}

fn fixed_bytes<const N: usize>(value: &[u8], data_type: &Type) -> Result<[u8; N], String> {
    value.try_into().map_err(|_| {
        format!(
            "invalid binary {} parameter length: expected {}, got {}",
            data_type.name(),
            N,
            value.len()
        )
    })
}

fn pg_epoch_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("valid PG epoch date")
}

fn pg_epoch_datetime() -> chrono::NaiveDateTime {
    pg_epoch_date()
        .and_hms_opt(0, 0, 0)
        .expect("valid PG epoch timestamp")
}

fn format_time_micros(micros: i64) -> Result<String, String> {
    let micros_per_day = 86_400_000_000_i64;
    if !(0..micros_per_day).contains(&micros) {
        return Err(format!("time SQL parameter is out of range: {micros}"));
    }
    let seconds = micros / 1_000_000;
    let nanos = (micros % 1_000_000) as u32 * 1_000;
    chrono::NaiveTime::from_num_seconds_from_midnight_opt(seconds as u32, nanos)
        .map(|time| time.format("%H:%M:%S%.6f").to_string())
        .ok_or_else(|| format!("time SQL parameter is out of range: {micros}"))
}

fn metadata_value<'a, C>(client: &'a C, key: &str) -> &'a str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(key)
        .map(|value| value.as_str())
        .unwrap_or("")
}

fn session_user<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    client
        .metadata()
        .get(METADATA_USER)
        .map(|value| value.as_str())
        .unwrap_or("admin")
}

fn session_database<C>(client: &C) -> &str
where
    C: ClientInfo,
{
    metadata_value(client, METADATA_DATABASE)
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
                match crate::db::authenticate_user(username, password).await {
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

#[async_trait]
impl SimpleQueryHandler for DuckdbProcessor {
    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = query.trim().to_string();
        if sql.is_empty() {
            debug!(target: "rsduck::pg", protocol = "simple", "PG empty query");
            return Ok(vec![Response::EmptyQuery]);
        }

        let current_user = session_user(client);
        let current_database = session_database(client);
        debug!(
            target: "rsduck::pg",
            protocol = "simple",
            user = %current_user,
            database = %current_database,
            sql = %sql,
            "PG query"
        );
        match execute_pg_sql(sql.clone(), current_user, Vec::new()).await {
            Ok(SqlTypedResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                Ok(vec![query_to_response(columns, rows, FieldFormat::Text)?])
            }
            Ok(SqlTypedResult::Execute {
                command,
                affected_rows,
            }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    command = %command,
                    affected_rows,
                    "PG execute result"
                );
                Ok(vec![exec_to_response(&command, affected_rows)])
            }
            Err(e) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "simple",
                    user = %current_user,
                    database = %current_database,
                    sql = %sql,
                    error = %e,
                    "PG query failed"
                );
                Err(PgWireError::ApiError(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e,
                ))))
            }
        }
    }
}

#[async_trait]
impl ExtendedQueryHandler for DuckdbProcessor {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn on_parse<C>(&self, client: &mut C, message: Parse) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let provided_types = message
            .type_oids
            .iter()
            .map(|oid| Type::from_oid(*oid).unwrap_or(Type::UNKNOWN))
            .collect::<Vec<_>>();
        let parameter_types = infer_parameter_types(&message.query, &provided_types);
        let id = message
            .name
            .clone()
            .unwrap_or_else(|| DEFAULT_NAME.to_string());
        let statement = StoredStatement::new(id, message.query, parameter_types);
        client.portal_store().put_statement(Arc::new(statement));
        client
            .send(PgWireBackendMessage::ParseComplete(ParseComplete::new()))
            .await?;
        Ok(())
    }

    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        statement: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %session_user(client),
            database = %session_database(client),
            sql = %statement.statement,
            "PG describe statement"
        );
        let parameter_types = infer_statement_parameter_types(statement);
        let describe_params = parameter_types
            .iter()
            .map(dummy_sql_param_for_type)
            .collect();
        let fields = describe_pg_sql(
            statement.statement.to_string(),
            session_user(client),
            describe_params,
        )
        .await
        .map(|columns| fields_for_columns(columns, FieldFormat::Binary))
        .map_err(pg_api_error)?;
        Ok(DescribeStatementResponse::new(parameter_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %session_user(client),
            database = %session_database(client),
            sql = %portal.statement.statement,
            "PG describe portal"
        );
        let params = sql_params_from_portal(portal).map_err(pg_api_error)?;
        let fields = describe_pg_sql(
            portal.statement.statement.to_string(),
            session_user(client),
            params,
        )
        .await
        .map(|columns| fields_for_columns(columns, FieldFormat::Binary))
        .map_err(pg_api_error)?;
        Ok(DescribePortalResponse::new(fields))
    }

    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let sql = portal.statement.statement.to_string();

        let current_user = session_user(client);
        let current_database = session_database(client);
        debug!(
            target: "rsduck::pg",
            protocol = "extended",
            user = %current_user,
            database = %current_database,
            sql = %sql,
            "PG query"
        );
        let params = sql_params_from_portal(portal).map_err(pg_api_error)?;
        match execute_pg_sql(sql.clone(), current_user, params).await {
            Ok(SqlTypedResult::Query { columns, rows }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    column_count = columns.len(),
                    row_count = rows.len(),
                    "PG query result"
                );
                query_to_response(columns, rows, FieldFormat::Binary)
            }
            Ok(SqlTypedResult::Execute {
                command,
                affected_rows,
            }) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    command = %command,
                    affected_rows,
                    "PG execute result"
                );
                Ok(exec_to_response(&command, affected_rows))
            }
            Err(e) => {
                debug!(
                    target: "rsduck::pg",
                    protocol = "extended",
                    user = %current_user,
                    database = %current_database,
                    sql = %sql,
                    error = %e,
                    "PG query failed"
                );
                Err(pg_api_error(e))
            }
        }
    }
}

fn pg_api_error(message: String) -> PgWireError {
    PgWireError::ApiError(Box::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        message,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_explicit_cast_parameter_types() {
        assert_eq!(
            infer_cast_type_for_placeholder("select $1::varchar as ok", 1),
            Some(Type::VARCHAR)
        );
        assert_eq!(
            infer_cast_type_for_placeholder("select $2 :: integer as id", 2),
            Some(Type::INT4)
        );
        assert_eq!(infer_cast_type_for_placeholder("select '$1'", 1), None);
    }

    #[test]
    fn decodes_binary_pg_parameters_for_sql_binding() {
        assert_eq!(
            sql_param_from_pg_value(Some(&1_i32.to_be_bytes()), &Type::INT4, FieldFormat::Binary)
                .unwrap(),
            SqlParam::Integer(1)
        );
        assert_eq!(
            sql_param_from_pg_value(Some(b"ready"), &Type::TEXT, FieldFormat::Binary).unwrap(),
            SqlParam::Text("ready".to_string())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_postgres_extended_query_handles_params_and_typed_rows() {
        let cfg = crate::config::DbConfig {
            read_workers: 1,
            write_queue_size: 8,
            read_queue_size: 8,
            snapshot_queue_size: 1,
            max_result_rows: 100,
            ..crate::config::DbConfig::default()
        };
        crate::db::init_db(None, &cfg);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_pg_listener(listener).await;
        });

        let connect_string = format!(
            "host=127.0.0.1 port={} user=admin password=admin dbname=memory",
            addr.port()
        );
        let (client, connection) = tokio_postgres::connect(&connect_string, tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
        });

        let ok = "1";
        let rows = client
            .query("select $1::varchar as ok", &[&ok])
            .await
            .unwrap();
        let text_value: String = rows[0].try_get(0).unwrap();
        assert_eq!(text_value, "1");

        let rows = client.query("select 1::integer as ok", &[]).await.unwrap();
        let int_value: i32 = rows[0].try_get(0).unwrap();
        assert_eq!(int_value, 1);

        let param_value = 2_i32;
        let rows = client
            .query("select $1::integer as ok", &[&param_value])
            .await
            .unwrap();
        let int_param_value: i32 = rows[0].try_get(0).unwrap();
        assert_eq!(int_param_value, 2);

        connection_task.abort();
        server.abort();
        crate::db::shutdown_workers();
    }
}

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

pub async fn start_pg_server(bind: &str) {
    let listener = TcpListener::bind(bind)
        .await
        .expect("bind pg server failed");
    serve_pg_listener(listener).await;
}

async fn serve_pg_listener(listener: TcpListener) {
    let bind = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    info!("PG wire protocol listening on {}", bind);

    let factory = Arc::new(DuckdbHandlerFactory {
        handler: Arc::new(DuckdbProcessor::new()),
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

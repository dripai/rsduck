use std::sync::Arc;

use futures::stream;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::Type;
use pgwire::error::{PgWireError, PgWireResult};

use crate::db::SqlColumn;

use super::params::{parse_pg_bool_text, parse_pg_bytea_text};

pub(super) fn query_to_response<'a>(
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

pub(super) fn exec_to_response<'a>(command: &str, affected_rows: usize) -> Response<'a> {
    Response::Execution(Tag::new(command).with_rows(affected_rows))
}

pub(super) fn fields_for_columns(columns: Vec<SqlColumn>, format: FieldFormat) -> Vec<FieldInfo> {
    columns
        .into_iter()
        .map(|c| {
            let pg_type = Type::from_oid(c.data_type.pg_type_oid()).unwrap_or(Type::TEXT);
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

fn parse_pg_bool(value: &str) -> PgWireResult<bool> {
    parse_pg_bool_text(value).map_err(pg_api_error)
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

pub(super) fn pg_api_error(message: String) -> PgWireError {
    PgWireError::ApiError(Box::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        message,
    )))
}

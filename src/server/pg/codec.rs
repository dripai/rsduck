use std::error::Error;
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use futures::stream;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::Type;
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::types::ToSqlText;
use postgres_types::{IsNull, ToSql};

use crate::db::{SqlColumn, SqlValue};

pub(super) fn query_to_response<'a>(
    columns: Vec<SqlColumn>,
    rows: Vec<Vec<SqlValue>>,
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
                encode_pg_cell(&mut encoder, cell, field.datatype(), field.format())?;
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
    value: SqlValue,
    data_type: &Type,
    format: FieldFormat,
) -> PgWireResult<()> {
    if matches!(value, SqlValue::Null) {
        return encoder.encode_field_with_type_and_format(&None::<i8>, data_type, format);
    }

    if format == FieldFormat::Text {
        let text = value.text_value().unwrap_or_default();
        return encoder.encode_field_with_type_and_format(&text, data_type, format);
    }

    match *data_type {
        Type::BOOL => {
            let value = bool_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT2 => {
            let value = int16_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT4 => {
            let value = int32_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::INT8 => {
            let value = int64_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::FLOAT4 => {
            let value = float32_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::FLOAT8 => {
            let value = float64_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            let value = value.text_value().unwrap_or_default();
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::DATE => {
            let value = date_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIME => {
            let value = time_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIMESTAMP => {
            let value = timestamp_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::TIMESTAMPTZ => {
            let value = timestamptz_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::BYTEA => {
            let value = bytes_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::NUMERIC => {
            let value = decimal_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&value, data_type, format)
        }
        Type::UUID => {
            let value = uuid_value(value, data_type)?;
            encoder.encode_field_with_type_and_format(&PgUuid(value), data_type, format)
        }
        _ => Err(pg_api_error(format!(
            "unsupported PG binary column type: {}",
            data_type.name()
        ))),
    }
}

fn bool_value(value: SqlValue, data_type: &Type) -> PgWireResult<bool> {
    match value {
        SqlValue::Bool(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn int16_value(value: SqlValue, data_type: &Type) -> PgWireResult<i16> {
    match value {
        SqlValue::Int16(value) => Ok(value),
        SqlValue::Int32(value) => {
            i16::try_from(value).map_err(|_| type_mismatch_i64(value as i64, data_type))
        }
        SqlValue::Int64(value) => {
            i16::try_from(value).map_err(|_| type_mismatch_i64(value, data_type))
        }
        other => Err(type_mismatch(other, data_type)),
    }
}

fn int32_value(value: SqlValue, data_type: &Type) -> PgWireResult<i32> {
    match value {
        SqlValue::Int16(value) => Ok(value as i32),
        SqlValue::Int32(value) => Ok(value),
        SqlValue::Int64(value) => {
            i32::try_from(value).map_err(|_| type_mismatch_i64(value, data_type))
        }
        other => Err(type_mismatch(other, data_type)),
    }
}

fn int64_value(value: SqlValue, data_type: &Type) -> PgWireResult<i64> {
    match value {
        SqlValue::Int16(value) => Ok(value as i64),
        SqlValue::Int32(value) => Ok(value as i64),
        SqlValue::Int64(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn float32_value(value: SqlValue, data_type: &Type) -> PgWireResult<f32> {
    match value {
        SqlValue::Float32(value) => Ok(value),
        SqlValue::Float64(value) => Ok(value as f32),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn float64_value(value: SqlValue, data_type: &Type) -> PgWireResult<f64> {
    match value {
        SqlValue::Float32(value) => Ok(value as f64),
        SqlValue::Float64(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn date_value(value: SqlValue, data_type: &Type) -> PgWireResult<chrono::NaiveDate> {
    match value {
        SqlValue::Date(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn time_value(value: SqlValue, data_type: &Type) -> PgWireResult<chrono::NaiveTime> {
    match value {
        SqlValue::Time(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn timestamp_value(value: SqlValue, data_type: &Type) -> PgWireResult<chrono::NaiveDateTime> {
    match value {
        SqlValue::Timestamp(value) => Ok(value),
        SqlValue::TimestampTz(value) => Ok(value.naive_utc()),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn timestamptz_value(
    value: SqlValue,
    data_type: &Type,
) -> PgWireResult<chrono::DateTime<chrono::Utc>> {
    match value {
        SqlValue::TimestampTz(value) => Ok(value),
        SqlValue::Timestamp(value) => Ok(
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(value, chrono::Utc),
        ),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn bytes_value(value: SqlValue, data_type: &Type) -> PgWireResult<Vec<u8>> {
    match value {
        SqlValue::Bytes(value) => Ok(value),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn decimal_value(value: SqlValue, data_type: &Type) -> PgWireResult<rust_decimal::Decimal> {
    match value {
        SqlValue::Decimal(value) => Ok(value),
        SqlValue::NumericText(value) => value.parse::<rust_decimal::Decimal>().map_err(|e| {
            pg_api_error(format!(
                "failed to encode numeric value '{value}' as {}: {e}",
                data_type.name()
            ))
        }),
        SqlValue::Int16(value) => Ok(rust_decimal::Decimal::from(value)),
        SqlValue::Int32(value) => Ok(rust_decimal::Decimal::from(value)),
        SqlValue::Int64(value) => Ok(rust_decimal::Decimal::from(value)),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn uuid_value(value: SqlValue, data_type: &Type) -> PgWireResult<uuid::Uuid> {
    match value {
        SqlValue::Uuid(value) => Ok(value),
        SqlValue::Text(value) => uuid::Uuid::parse_str(&value).map_err(|e| {
            pg_api_error(format!(
                "failed to encode uuid value '{value}' as {}: {e}",
                data_type.name()
            ))
        }),
        other => Err(type_mismatch(other, data_type)),
    }
}

fn type_mismatch(value: SqlValue, data_type: &Type) -> PgWireError {
    pg_api_error(format!(
        "cannot encode value {:?} as PG binary {}",
        value,
        data_type.name()
    ))
}

fn type_mismatch_i64(value: i64, data_type: &Type) -> PgWireError {
    pg_api_error(format!(
        "integer value {value} is out of range for PG binary {}",
        data_type.name()
    ))
}

#[derive(Debug)]
struct PgUuid(uuid::Uuid);

impl ToSqlText for PgUuid {
    fn to_sql_text(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>>
    where
        Self: Sized,
    {
        if *ty != Type::UUID {
            return Err(Box::new(postgres_types::WrongType::new::<PgUuid>(
                ty.clone(),
            )));
        }
        out.put_slice(self.0.to_string().as_bytes());
        Ok(IsNull::No)
    }
}

impl ToSql for PgUuid {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Sync + Send>>
    where
        Self: Sized,
    {
        if *ty != Type::UUID {
            return Err(Box::new(postgres_types::WrongType::new::<PgUuid>(
                ty.clone(),
            )));
        }
        out.put_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool
    where
        Self: Sized,
    {
        *ty == Type::UUID
    }

    postgres_types::to_sql_checked!();
}

pub(super) fn pg_api_error(message: String) -> PgWireError {
    PgWireError::ApiError(Box::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        message,
    )))
}

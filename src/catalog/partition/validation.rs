use super::*;

pub(in crate::catalog) fn validate_partition_key(
    create_table: &CreateTable,
    partition_key: &str,
    partition_unit: &str,
) -> Result<(String, i64), String> {
    if !matches!(partition_unit, "hour" | "day" | "month" | "year") {
        return Err(format!(
            "partition_unit must be one of hour, day, month, year: {partition_unit}"
        ));
    }

    let column = create_table
        .columns
        .iter()
        .find(|column| column.name.value.eq_ignore_ascii_case(partition_key))
        .ok_or_else(|| format!("partition key column does not exist: {partition_key}"))?;
    if !column
        .options
        .iter()
        .any(|option| matches!(option.option, ColumnOption::NotNull))
    {
        return Err("partition key column must be NOT NULL".into());
    }

    let type_text = column.data_type.to_string();
    let type_lower = type_text.to_ascii_lowercase();
    let key_type = if type_lower == "date" {
        if partition_unit == "hour" {
            return Err("DATE partition key does not support partition_unit = 'hour'".into());
        }
        "date"
    } else if type_lower.starts_with("timestamp") || type_lower == "datetime" {
        "timestamp"
    } else {
        return Err(format!(
            "partition key must be DATE or TIMESTAMP, got {type_text}"
        ));
    };
    Ok((key_type.to_string(), type_id_for_duckdb_type(&type_text)?))
}

pub(in crate::catalog) fn validate_create_table_column_types(
    create_table: &CreateTable,
) -> Result<(), String> {
    for column in &create_table.columns {
        let type_text = column.data_type.to_string();
        type_id_for_duckdb_type(&type_text)?;
    }
    Ok(())
}

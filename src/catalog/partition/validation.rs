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
    let complex_columns = create_table
        .columns
        .iter()
        .filter_map(|column| {
            let type_text = column.data_type.to_string();
            if is_complex_duckdb_type(&type_text) {
                Some(column.name.value.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for column in &create_table.columns {
        let type_text = column.data_type.to_string();
        validate_supported_column_type(&type_text)?;
        if is_complex_duckdb_type(&type_text) {
            for option in &column.options {
                let option_text = option.option.to_string();
                let option_lower = option_text.to_ascii_lowercase();
                if option_lower.contains("primary key") {
                    return Err(format!(
                        "complex column cannot be used as primary key: {}",
                        column.name
                    ));
                }
                if option_lower.starts_with("unique") || option_lower.contains(" unique") {
                    return Err(format!(
                        "complex column cannot be used as unique key: {}",
                        column.name
                    ));
                }
                if option_lower.contains("references") || option_lower.contains("foreign key") {
                    return Err(format!(
                        "complex column cannot be used as foreign key: {}",
                        column.name
                    ));
                }
                if option_lower.starts_with("check")
                    || option_lower.contains(" check")
                    || option_lower.contains("constraint")
                {
                    return Err(format!(
                        "CHECK constraint on complex column is not supported: {}",
                        column.name
                    ));
                }
                if option_lower.starts_with("default")
                    && !option_lower.trim().eq_ignore_ascii_case("default null")
                {
                    return Err(format!(
                        "complex column does not support non-NULL default value: {}",
                        column.name
                    ));
                }
            }
        }
    }

    for constraint in &create_table.constraints {
        match constraint {
            TableConstraint::PrimaryKey(pk) => {
                reject_complex_index_columns(&pk.columns, &complex_columns, "primary key")?;
            }
            TableConstraint::Unique(unique) => {
                reject_complex_index_columns(&unique.columns, &complex_columns, "unique key")?;
            }
            TableConstraint::ForeignKey(fk) => {
                for column in &fk.columns {
                    if complex_columns
                        .iter()
                        .any(|complex| complex.eq_ignore_ascii_case(&column.value))
                    {
                        return Err(format!(
                            "complex column cannot be used as foreign key: {}",
                            column.value
                        ));
                    }
                }
            }
            TableConstraint::Check(check) => {
                let expr = check.expr.to_string();
                for column in &complex_columns {
                    if expression_references_column(&expr, column) {
                        return Err(format!(
                            "CHECK constraint cannot reference complex column: {column}"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(in crate::catalog) fn validate_column_def_type(
    column: &sqlparser::ast::ColumnDef,
) -> Result<(), String> {
    let type_text = column.data_type.to_string();
    validate_supported_column_type(&type_text)?;
    if !is_complex_duckdb_type(&type_text) {
        return Ok(());
    }
    for option in &column.options {
        let option_text = option.option.to_string();
        let option_lower = option_text.to_ascii_lowercase();
        if option_lower.contains("primary key") {
            return Err(format!(
                "complex column cannot be used as primary key: {}",
                column.name
            ));
        }
        if option_lower.starts_with("unique") || option_lower.contains(" unique") {
            return Err(format!(
                "complex column cannot be used as unique key: {}",
                column.name
            ));
        }
        if option_lower.contains("references") || option_lower.contains("foreign key") {
            return Err(format!(
                "complex column cannot be used as foreign key: {}",
                column.name
            ));
        }
        if option_lower.starts_with("check")
            || option_lower.contains(" check")
            || option_lower.contains("constraint")
        {
            return Err(format!(
                "CHECK constraint on complex column is not supported: {}",
                column.name
            ));
        }
        if option_lower.starts_with("default")
            && !option_lower.trim().eq_ignore_ascii_case("default null")
        {
            return Err(format!(
                "complex column does not support non-NULL default value: {}",
                column.name
            ));
        }
    }
    Ok(())
}

fn reject_complex_index_columns(
    columns: &[sqlparser::ast::IndexColumn],
    complex_columns: &[String],
    usage: &str,
) -> Result<(), String> {
    for column in columns {
        let column_name = column.column.expr.to_string();
        if complex_columns
            .iter()
            .any(|complex| complex.eq_ignore_ascii_case(&column_name))
        {
            return Err(format!(
                "complex column cannot be used as {usage}: {column_name}"
            ));
        }
    }
    Ok(())
}

fn expression_references_column(expr: &str, column: &str) -> bool {
    expr.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|token| token.eq_ignore_ascii_case(column))
}

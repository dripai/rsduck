use super::*;

pub(super) fn schema_name_value(schema_name: &SchemaName) -> Result<String, String> {
    match schema_name {
        SchemaName::Simple(name) | SchemaName::NamedAuthorization(name, _) => {
            single_name_part(name)
        }
        SchemaName::UnnamedAuthorization(ident) => Ok(ident.value.clone()),
    }
}

pub(super) fn relation_name(name: &ObjectName) -> Result<(String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation] => Ok(("main".to_string(), relation.clone())),
        [schema, relation] => Ok((schema.clone(), relation.clone())),
        _ => Err(format!("unsupported relation name: {name}")),
    }
}

pub(super) fn relation_name_with_default(
    name: &ObjectName,
    default_schema: &str,
) -> Result<(String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation] => Ok((default_schema.to_string(), relation.clone())),
        [schema, relation] => Ok((schema.clone(), relation.clone())),
        _ => Err(format!("unsupported relation name: {name}")),
    }
}

pub(super) fn comment_relation_name(
    object_type: CommentObject,
    object_name: &ObjectName,
) -> Result<(String, String), String> {
    if object_type == CommentObject::Column {
        let (schema, relation, _) = column_comment_target(object_name)?;
        Ok((schema, relation))
    } else {
        relation_name(object_name)
    }
}

pub(super) fn column_comment_target(name: &ObjectName) -> Result<(String, String, String), String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [relation, column] => Ok(("main".to_string(), relation.clone(), column.clone())),
        [schema, relation, column] => Ok((schema.clone(), relation.clone(), column.clone())),
        _ => Err(format!("unsupported column name for COMMENT: {name}")),
    }
}

pub(super) fn single_name_part(name: &ObjectName) -> Result<String, String> {
    let parts = ident_parts(name)?;
    match parts.as_slice() {
        [part] => Ok(part.clone()),
        _ => Err(format!("unsupported schema name: {name}")),
    }
}

pub(super) fn ident_parts(name: &ObjectName) -> Result<Vec<String>, String> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => Ok(ident.value.clone()),
            _ => Err(format!("unsupported object name part: {part}")),
        })
        .collect()
}

pub(super) fn reject_reserved_schema(schema: &str) -> Result<(), String> {
    if is_reserved_schema(schema) {
        Err(format!(
            "reserved schema is managed by rsduck catalog: {schema}"
        ))
    } else {
        Ok(())
    }
}

pub(super) fn is_reserved_schema(schema: &str) -> bool {
    matches!(
        schema.to_ascii_lowercase().as_str(),
        "pg_catalog" | "information_schema" | "rsduck_catalog" | "rsduck_internal"
    )
}

pub(super) fn normalize_for_guard(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .to_ascii_lowercase()
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn statement_command(statement: &Statement) -> &'static str {
    match statement {
        Statement::CreateSchema { .. }
        | Statement::CreateTable(_)
        | Statement::CreateView(_)
        | Statement::CreateIndex(_)
        | Statement::CreateUser(_) => "CREATE",
        Statement::Drop { .. } => "DROP",
        Statement::AlterTable(_)
        | Statement::AlterSchema(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. }
        | Statement::AlterUser(_) => "ALTER",
        Statement::Comment { .. } => "COMMENT",
        Statement::Grant(_) => "GRANT",
        Statement::Revoke(_) => "REVOKE",
        _ => "SQL",
    }
}

pub(super) fn sql_bool(value: bool) -> &'static str {
    if value {
        "TRUE"
    } else {
        "FALSE"
    }
}

pub(super) fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

pub(super) fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(super) fn quote_qualified(schema: &str, relation: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(relation))
}

use super::*;

pub fn execute_init_sql(conn: &Connection, sql: &str) -> Result<(), String> {
    let dialect = DuckDbDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| format!("init_sql parse failed: {e}"))?;
    for statement in statements {
        let sql = statement.to_string();
        execute_catalog_statement(conn, &statement, &sql, ADMIN_USER_ID)?;
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn execute_catalog_aware_write(conn: &Connection, sql: &str) -> Result<Option<usize>, String> {
    execute_catalog_aware_write_as(conn, "admin", sql)
}

pub fn execute_catalog_aware_write_as(
    conn: &Connection,
    username: &str,
    sql: &str,
) -> Result<Option<usize>, String> {
    let principal = principal_for_username(conn, username)?;
    if let Some(call) = parse_catalog_management_call(sql) {
        return execute_catalog_management_call(conn, &principal, call, sql).map(Some);
    }
    if let Some(partitioned) = parse_managed_partition_create(sql)? {
        return create_range_partitioned_table(conn, &partitioned, principal.user_id).map(Some);
    }
    let (statement, normalized_sql) = parse_one_statement(sql)?;
    execute_catalog_statement(conn, &statement, &normalized_sql, principal.user_id)
}

pub(in crate::catalog) fn execute_catalog_statement(
    conn: &Connection,
    statement: &Statement,
    sql: &str,
    owner_user_id: i64,
) -> Result<Option<usize>, String> {
    match statement {
        Statement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => create_schema(conn, schema_name, *if_not_exists, owner_user_id).map(Some),
        Statement::CreateUser(create_user) => {
            create_user_account(conn, create_user, sql, owner_user_id).map(Some)
        }
        Statement::CreateRole(create_role) => {
            create_role_account(conn, create_role, sql, owner_user_id).map(Some)
        }
        Statement::AlterUser(alter_user) => {
            alter_user_account(conn, alter_user, sql, owner_user_id).map(Some)
        }
        Statement::CreateTable(create_table) => {
            create_table_relation(conn, create_table, sql, owner_user_id).map(Some)
        }
        Statement::CreateView(create_view) => {
            create_view_relation(conn, create_view, sql, owner_user_id).map(Some)
        }
        Statement::CreateIndex(create_index) => {
            create_index_relation(conn, create_index, sql, owner_user_id).map(Some)
        }
        Statement::AlterTable(alter_table) => {
            alter_table_relation(conn, alter_table, sql, owner_user_id).map(Some)
        }
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            table,
            ..
        } => drop_objects(
            conn,
            *object_type,
            *if_exists,
            names,
            *cascade,
            table.as_ref(),
            sql,
        )
        .map(Some),
        Statement::Comment {
            object_type,
            object_name,
            comment,
            if_exists,
        } => comment_object(
            conn,
            *object_type,
            object_name,
            comment.as_deref(),
            *if_exists,
            sql,
        )
        .map(Some),
        Statement::Insert(insert) => {
            insert_partitioned_relation(conn, insert, sql).map(|affected| {
                if affected == 0 {
                    None
                } else {
                    Some(affected)
                }
            })
        }
        Statement::Grant(grant) => grant_privileges(conn, grant, sql, owner_user_id).map(Some),
        Statement::Revoke(revoke) => revoke_privileges(conn, revoke, sql).map(Some),
        Statement::AlterSchema(_) | Statement::AlterIndex { .. } | Statement::AlterView { .. } => {
            Err(format!(
                "catalog mutation is not implemented for this statement: {}",
                statement_command(statement)
            ))
        }
        _ => {
            guard_external_sql(sql)?;
            Ok(None)
        }
    }
}

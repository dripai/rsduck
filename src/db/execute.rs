fn execute_sql_blocking(
    conn: &Connection,
    username: &str,
    sql: &str,
    route: SqlRoute,
    command: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if let Some(result) = crate::pg_compat::compat_result(sql_trimmed, username) {
        return Ok(result);
    }
    if let Some(rewritten_sql) = crate::pg_compat::rewrite_sql(sql_trimmed) {
        crate::catalog::authorize_catalog_projection(conn, username)?;
        return query_sql_blocking(conn, &rewritten_sql, max_result_rows);
    }
    if crate::catalog::is_reserved_diagnostic_read(sql_trimmed) {
        crate::catalog::authorize_reserved_diagnostic(conn, username, sql_trimmed)?;
        return query_sql_blocking(conn, sql_trimmed, max_result_rows);
    }
    crate::catalog::guard_external_sql_as(username, sql_trimmed)?;
    crate::catalog::reject_unhandled_catalog_projection(sql_trimmed)?;
    crate::catalog::authorize_sql(conn, username, sql_trimmed)?;

    match route {
        SqlRoute::Read => query_sql_blocking(conn, sql_trimmed, max_result_rows),
        SqlRoute::Write => {
            let affected_rows = match crate::catalog::execute_catalog_aware_write_as(
                conn,
                username,
                sql_trimmed,
            )? {
                Some(affected_rows) => affected_rows,
                None => conn.execute(sql_trimmed, []).map_err(|e| e.to_string())?,
            };
            Ok(SqlResult::Execute {
                command: command.to_string(),
                affected_rows,
            })
        }
    }
}

fn query_sql_blocking(
    conn: &Connection,
    sql: &str,
    max_result_rows: usize,
) -> Result<SqlResult, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "query did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    let cols: Vec<String> = (0..col_count)
        .map(|idx| {
            stmt_ref
                .column_name(idx)
                .map(|name| name.to_string())
                .unwrap_or_else(|_| format!("column_{idx}"))
        })
        .collect();
    let mut data = Vec::new();

    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if data.len() >= max_result_rows {
            return Err(format!("result row limit exceeded: {max_result_rows}"));
        }
        let mut line = Vec::with_capacity(cols.len());
        for idx in 0..cols.len() {
            line.push(cell_to_string(row, idx));
        }
        data.push(line);
    }

    Ok(SqlResult::Query {
        columns: cols,
        rows: data,
    })
}


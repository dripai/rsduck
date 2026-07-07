fn partition_insert_groups(
    conn: &Connection,
    source: &sqlparser::ast::Query,
    target_columns: &[String],
    partition_key_idx: Option<usize>,
    relation: &PartitionedRelation,
) -> Result<PartitionInsertGroups, String> {
    if let SetExpr::Values(values) = source.body.as_ref() {
        let mut groups = Vec::new();
        for row in &values.rows {
            if row.content.len() != target_columns.len() {
                return Err(format!(
                    "INSERT column count mismatch: target={}, row={}",
                    target_columns.len(),
                    row.content.len()
                ));
            }
            let idx = partition_key_idx.ok_or_else(|| {
                format!(
                    "INSERT into managed partitioned table requires partition key column: {}",
                    relation.partition_key
                )
            })?;
            let route = partition_route_for_expr(
                &row.content[idx],
                &relation.partition_key_type,
                &relation.partition_unit,
            )?;
            let exprs = row
                .content
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            push_partition_insert_group(&mut groups, route, exprs);
        }
        return Ok(groups);
    }

    materialize_query_partition_insert_groups(
        conn,
        source,
        target_columns,
        partition_key_idx,
        relation,
    )
}

fn materialize_query_partition_insert_groups(
    conn: &Connection,
    source: &sqlparser::ast::Query,
    target_columns: &[String],
    partition_key_idx: Option<usize>,
    relation: &PartitionedRelation,
) -> Result<PartitionInsertGroups, String> {
    let query_sql = format!("SELECT * FROM ({source}) AS rsduck_insert_source");
    let mut stmt = conn
        .prepare(&query_sql)
        .map_err(|e| format!("prepare partition INSERT source query failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition INSERT source failed: {e}"))?;
    let stmt_ref = rows
        .as_ref()
        .ok_or_else(|| "partition INSERT source did not expose statement metadata".to_string())?;
    let col_count = stmt_ref.column_count();
    if col_count != target_columns.len() {
        return Err(format!(
            "INSERT source column count mismatch: target={}, source={col_count}",
            target_columns.len()
        ));
    }

    let mut groups = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition INSERT source failed: {e}"))?
    {
        let idx = partition_key_idx.ok_or_else(|| {
            format!(
                "INSERT into managed partitioned table requires partition key column: {}",
                relation.partition_key
            )
        })?;
        let value = row
            .get_ref(idx)
            .map_err(|e| format!("read partition key source value failed: {e}"))?;
        let route = partition_route_for_value_ref(
            value,
            &relation.partition_key_type,
            &relation.partition_unit,
        )?;
        let mut exprs = Vec::with_capacity(col_count);
        for idx in 0..col_count {
            let value = row
                .get_ref(idx)
                .map_err(|e| format!("read partition INSERT source value failed: {e}"))?;
            exprs.push(value_ref_to_sql_literal(value)?);
        }
        push_partition_insert_group(&mut groups, route, exprs);
    }
    Ok(groups)
}

fn push_partition_insert_group(
    groups: &mut PartitionInsertGroups,
    route: PartitionRoute,
    exprs: Vec<String>,
) {
    if let Some((_, existing_ts, rows)) = groups
        .iter_mut()
        .find(|(partition_value, _, _)| partition_value == &route.partition_value)
    {
        if existing_ts.is_none() {
            *existing_ts = route.route_ts;
        }
        rows.push(exprs);
    } else {
        groups.push((route.partition_value, route.route_ts, vec![exprs]));
    }
}

fn partition_route_for_value_ref(
    value: ValueRef<'_>,
    partition_key_type: &str,
    partition_unit: &str,
) -> Result<PartitionRoute, String> {
    let route_ts = partition_datetime_from_value_ref(value, partition_key_type);
    match route_ts {
        Some(dt) => Ok(PartitionRoute {
            partition_value: partition_value_for_datetime(dt, partition_unit),
            route_ts: Some(dt),
        }),
        None => Err("partition key value is NULL or cannot be routed".into()),
    }
}

fn partition_datetime_from_value_ref(
    value: ValueRef<'_>,
    partition_key_type: &str,
) -> Option<NaiveDateTime> {
    match value {
        ValueRef::Null => None,
        ValueRef::Timestamp(unit, value) => timestamp_value_to_naive(unit, value),
        ValueRef::Date32(value) => date32_value_to_naive(value),
        ValueRef::Text(value) => std::str::from_utf8(value)
            .ok()
            .and_then(|text| parse_partition_datetime(text, partition_key_type)),
        _ => None,
    }
}

fn value_ref_to_sql_literal(value: ValueRef<'_>) -> Result<String, String> {
    match value {
        ValueRef::Null => Ok("NULL".to_string()),
        ValueRef::Boolean(v) => Ok(v.to_string()),
        ValueRef::TinyInt(v) => Ok(v.to_string()),
        ValueRef::SmallInt(v) => Ok(v.to_string()),
        ValueRef::Int(v) => Ok(v.to_string()),
        ValueRef::BigInt(v) => Ok(v.to_string()),
        ValueRef::HugeInt(v) => Ok(v.to_string()),
        ValueRef::UTinyInt(v) => Ok(v.to_string()),
        ValueRef::USmallInt(v) => Ok(v.to_string()),
        ValueRef::UInt(v) => Ok(v.to_string()),
        ValueRef::UBigInt(v) => Ok(v.to_string()),
        ValueRef::Float(v) => Ok(v.to_string()),
        ValueRef::Double(v) => Ok(v.to_string()),
        ValueRef::Decimal(v) => Ok(v.to_string()),
        ValueRef::Timestamp(unit, value) => {
            let dt = timestamp_value_to_naive(unit, value)
                .ok_or_else(|| "timestamp source value is out of range".to_string())?;
            Ok(format!(
                "TIMESTAMP '{}'",
                sql_string(&format_naive_datetime(dt))
            ))
        }
        ValueRef::Text(v) => Ok(format!("'{}'", sql_string(&String::from_utf8_lossy(v)))),
        ValueRef::Date32(value) => {
            let dt = date32_value_to_naive(value)
                .ok_or_else(|| "date source value is out of range".to_string())?;
            Ok(format!("DATE '{}'", dt.date()))
        }
        ValueRef::Time64(unit, value) => Ok(format!(
            "TIME '{}'",
            sql_string(&format_time64_value(unit, value)?)
        )),
        ValueRef::Blob(_) => Err("BLOB values are not supported in partition INSERT SELECT".into()),
        ValueRef::Interval { .. } => {
            Err("INTERVAL values are not supported in partition INSERT SELECT".into())
        }
        other => Err(format!(
            "unsupported value in partition INSERT SELECT: {other:?}"
        )),
    }
}

fn timestamp_value_to_naive(unit: TimeUnit, value: i64) -> Option<NaiveDateTime> {
    let (secs, nanos) = match unit {
        TimeUnit::Second => (value, 0),
        TimeUnit::Millisecond => (value / 1_000, (value % 1_000) * 1_000_000),
        TimeUnit::Microsecond => (value / 1_000_000, (value % 1_000_000) * 1_000),
        TimeUnit::Nanosecond => (value / 1_000_000_000, value % 1_000_000_000),
    };
    chrono::DateTime::from_timestamp(secs, nanos as u32).map(|dt| dt.naive_utc())
}

fn date32_value_to_naive(value: i32) -> Option<NaiveDateTime> {
    NaiveDate::from_ymd_opt(1970, 1, 1)?
        .checked_add_signed(Duration::days(i64::from(value)))?
        .and_hms_opt(0, 0, 0)
}

fn format_naive_datetime(dt: NaiveDateTime) -> String {
    format!(
        "{}.{:06}",
        dt.format("%Y-%m-%d %H:%M:%S"),
        dt.and_utc().timestamp_subsec_micros()
    )
}

fn format_time64_value(unit: TimeUnit, value: i64) -> Result<String, String> {
    let micros = match unit {
        TimeUnit::Microsecond => value,
        TimeUnit::Millisecond => value * 1_000,
        TimeUnit::Second => value * 1_000_000,
        TimeUnit::Nanosecond => value / 1_000,
    };
    if micros < 0 {
        return Err("negative TIME values are not supported in partition INSERT SELECT".into());
    }
    let seconds = micros / 1_000_000;
    let micros = micros % 1_000_000;
    Ok(format!(
        "{:02}:{:02}:{:02}.{:06}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        micros
    ))
}

fn partitioned_relation(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<Option<PartitionedRelation>, String> {
    let Some(meta) = find_relation_meta(conn, schema, table)? else {
        return Ok(None);
    };
    if meta.relkind != "p" {
        return Ok(None);
    }
    let (partition_key, partition_key_type, partition_unit, retention_count): (
        String,
        String,
        String,
        i32,
    ) = conn
        .query_row(
            &format!(
                "SELECT partition_key, partition_key_type, partition_unit, retention_count \
                 FROM rsduck_catalog.rs_relation_ext \
                 WHERE relid = {} AND managed_kind = 'range_partitioned_table'",
                meta.oid
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|e| format!("read partitioned relation metadata failed: {e}"))?;
    Ok(Some(PartitionedRelation {
        oid: meta.oid,
        schema: schema.to_string(),
        name: table.to_string(),
        partition_key,
        partition_key_type,
        partition_unit,
        retention_count,
        columns: catalog_columns(conn, meta.oid)?,
    }))
}

fn insert_target_columns(
    insert: &Insert,
    relation: &PartitionedRelation,
) -> Result<Vec<String>, String> {
    if insert.columns.is_empty() {
        return Ok(relation
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect());
    }
    insert
        .columns
        .iter()
        .map(single_name_part)
        .map(|result| {
            let column = result?;
            if relation
                .columns
                .iter()
                .any(|catalog_column| catalog_column.name.eq_ignore_ascii_case(&column))
            {
                Ok(column)
            } else {
                Err(format!("INSERT references unknown column: {column}"))
            }
        })
        .collect()
}

fn partition_route_for_expr(
    expr: &Expr,
    partition_key_type: &str,
    partition_unit: &str,
) -> Result<PartitionRoute, String> {
    let Some(dt) = partition_datetime_from_expr(expr, partition_key_type) else {
        return Err("partition key value is NULL or cannot be routed".into());
    };
    Ok(PartitionRoute {
        partition_value: partition_value_for_datetime(dt, partition_unit),
        route_ts: Some(dt),
    })
}

fn partition_datetime_from_expr(expr: &Expr, partition_key_type: &str) -> Option<NaiveDateTime> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Null => None,
            Value::SingleQuotedString(value)
            | Value::TripleSingleQuotedString(value)
            | Value::EscapedStringLiteral(value)
            | Value::UnicodeStringLiteral(value) => {
                parse_partition_datetime(value, partition_key_type)
            }
            _ => None,
        },
        Expr::TypedString(value) => match &value.value.value {
            Value::SingleQuotedString(text)
            | Value::TripleSingleQuotedString(text)
            | Value::EscapedStringLiteral(text)
            | Value::UnicodeStringLiteral(text) => {
                parse_partition_datetime(text, partition_key_type)
            }
            _ => None,
        },
        _ => None,
    }
}

fn parse_partition_datetime(value: &str, partition_key_type: &str) -> Option<NaiveDateTime> {
    let value = value.trim();
    if partition_key_type == "date" {
        return NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0));
    }
    parse_timestamp_literal(value)
}

fn parse_timestamp_literal(value: &str) -> Option<NaiveDateTime> {
    for pattern in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, pattern) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
}

fn partition_value_for_datetime(dt: NaiveDateTime, partition_unit: &str) -> String {
    match partition_unit {
        "hour" => format!(
            "{:04}{:02}{:02}{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ),
        "day" => format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()),
        "month" => format!("{:04}{:02}", dt.year(), dt.month()),
        "year" => format!("{:04}", dt.year()),
        _ => unreachable!("partition_unit is validated before routing"),
    }
}


fn ensure_active_partition(
    conn: &Connection,
    relation: &PartitionedRelation,
    partition_value: &str,
) -> Result<String, String> {
    if let Some(child) = active_partition_by_value(conn, relation.oid, partition_value)? {
        return Ok(child.relname);
    }
    if let Some(status) = partition_status_by_value(conn, relation.oid, partition_value)? {
        return Err(format!(
            "managed partition already exists with non-active status: {} partition_value={} status={}; explicit repair or retry is required",
            relation.name, partition_value, status
        ));
    }
    create_range_partition(conn, relation, partition_value)
}

fn active_partition_by_value(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
) -> Result<Option<ActivePartitionChild>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.child_relid, n.nspname, c.relname, c.status \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
               AND p.partition_value = '{}' \
               AND p.status = 'active'",
            sql_string(partition_value)
        ))
        .map_err(|e| format!("prepare partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(ActivePartitionChild {
        child_oid: row
            .get(0)
            .map_err(|e| format!("read partition child oid failed: {e}"))?,
        schema: row
            .get(1)
            .map_err(|e| format!("read partition schema failed: {e}"))?,
        relname: row
            .get(2)
            .map_err(|e| format!("read partition relation failed: {e}"))?,
        child_status: row
            .get(3)
            .map_err(|e| format!("read partition child status failed: {e}"))?,
    }))
}

fn partition_child_by_value(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
) -> Result<Option<ActivePartitionChild>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.child_relid, n.nspname, c.relname, c.status \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.pg_class c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
               AND p.partition_value = '{}'",
            sql_string(partition_value)
        ))
        .map_err(|e| format!("prepare partition child lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition child lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition child lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    Ok(Some(ActivePartitionChild {
        child_oid: row
            .get(0)
            .map_err(|e| format!("read partition child oid failed: {e}"))?,
        schema: row
            .get(1)
            .map_err(|e| format!("read partition child schema failed: {e}"))?,
        relname: row
            .get(2)
            .map_err(|e| format!("read partition child relation failed: {e}"))?,
        child_status: row
            .get(3)
            .map_err(|e| format!("read partition child status failed: {e}"))?,
    }))
}

fn partition_status_by_value(
    conn: &Connection,
    parent_oid: i64,
    partition_value: &str,
) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT status \
             FROM rsduck_catalog.rs_partition \
             WHERE parent_relid = {parent_oid} \
               AND partition_value = '{}' \
             ORDER BY child_relid DESC \
             LIMIT 1",
            sql_string(partition_value)
        ))
        .map_err(|e| format!("prepare partition status lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query partition status lookup failed: {e}"))?;
    let Some(row) = rows
        .next()
        .map_err(|e| format!("read partition status lookup failed: {e}"))?
    else {
        return Ok(None);
    };
    row.get(0)
        .map(Some)
        .map_err(|e| format!("read partition status failed: {e}"))
}

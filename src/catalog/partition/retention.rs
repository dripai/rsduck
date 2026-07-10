use super::*;

pub(in crate::catalog) fn retention_partitions(
    conn: &Connection,
    parent_oid: i64,
) -> Result<Vec<RetentionPartition>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.partition_value, n.nspname, c.relname, c.oid, c.reltype, c.relkind, c.relispartition \
             FROM rsduck_catalog.rs_partition p \
             JOIN rsduck_catalog.rs_relation c ON c.oid = p.child_relid \
             JOIN rsduck_catalog.rs_schema n ON n.oid = c.relnamespace \
             WHERE p.parent_relid = {parent_oid} \
               AND p.status = 'active' \
               AND p.is_null_partition = FALSE \
             ORDER BY p.partition_value"
        ))
        .map_err(|e| format!("prepare retention partition lookup failed: {e}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("query retention partition lookup failed: {e}"))?;
    let mut partitions = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read retention partition lookup failed: {e}"))?
    {
        partitions.push(RetentionPartition {
            partition_value: row
                .get(0)
                .map_err(|e| format!("read retention partition value failed: {e}"))?,
            schema: row
                .get(1)
                .map_err(|e| format!("read retention partition schema failed: {e}"))?,
            relname: row
                .get(2)
                .map_err(|e| format!("read retention partition relation failed: {e}"))?,
            meta: RelationMeta {
                oid: row
                    .get(3)
                    .map_err(|e| format!("read retention partition oid failed: {e}"))?,
                reltype: row
                    .get(4)
                    .map_err(|e| format!("read retention partition reltype failed: {e}"))?,
                relkind: row
                    .get(5)
                    .map_err(|e| format!("read retention partition relkind failed: {e}"))?,
                relispartition: row
                    .get(6)
                    .map_err(|e| format!("read retention partition relispartition failed: {e}"))?,
            },
        });
    }
    Ok(partitions)
}

pub(in crate::catalog) fn expire_partition(
    conn: &Connection,
    parent_oid: i64,
    partition: RetentionPartition,
) -> Result<(), String> {
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET status = 'expiring' \
             WHERE parent_relid = {parent_oid} AND child_relid = {} \
               AND is_null_partition = FALSE AND status = 'active'",
            partition.meta.oid
        ),
        [],
    )
    .map_err(|e| format!("mark partition expiring failed: {e}"))?;

    conn.execute(
        &format!(
            "DROP TABLE {}",
            quote_qualified(&partition.schema, &partition.relname)
        ),
        [],
    )
    .map_err(|e| {
        format!(
            "execute DuckDB DROP expired partition {}.{} failed: {e}",
            partition.schema, partition.relname
        )
    })?;
    delete_partition_child_catalog(conn, &partition.meta)?;
    conn.execute(
        &format!(
            "UPDATE rsduck_catalog.rs_partition \
             SET status = 'dropped', dropped_at = CURRENT_TIMESTAMP, error_message = '' \
             WHERE parent_relid = {parent_oid} AND child_relid = {} \
               AND partition_value = '{}'",
            partition.meta.oid,
            sql_string(&partition.partition_value)
        ),
        [],
    )
    .map_err(|e| format!("mark partition dropped failed: {e}"))?;
    Ok(())
}

pub(in crate::catalog) fn delete_partition_child_catalog(
    conn: &Connection,
    meta: &RelationMeta,
) -> Result<(), String> {
    for sql in [
        format!(
            "DELETE FROM rsduck_catalog.rs_dependency WHERE objid = {} OR refobjid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_comment WHERE objoid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_column_default WHERE adrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_column WHERE attrelid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_constraint WHERE conrelid = {} OR conindid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_index WHERE indexrelid = {} OR indrelid = {}",
            meta.oid, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_relation_ext WHERE relid = {}",
            meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_type WHERE oid = {} OR typrelid = {}",
            meta.reltype, meta.oid
        ),
        format!(
            "DELETE FROM rsduck_catalog.rs_relation WHERE oid = {}",
            meta.oid
        ),
    ] {
        conn.execute(&sql, [])
            .map_err(|e| format!("delete partition child catalog rows failed: {e}"))?;
    }
    Ok(())
}

pub(in crate::catalog) fn partition_bounds(
    partition_value: &str,
    partition_unit: &str,
) -> Result<PartitionBounds, String> {
    let lower_bound = match partition_unit {
        "hour" => NaiveDateTime::parse_from_str(partition_value, "%Y%m%d%H")
            .map_err(|_| format!("invalid hour partition_value: {partition_value}"))?,
        "day" => NaiveDate::parse_from_str(partition_value, "%Y%m%d")
            .map_err(|_| format!("invalid day partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid day partition_value: {partition_value}"))?,
        "month" => NaiveDate::parse_from_str(&format!("{partition_value}01"), "%Y%m%d")
            .map_err(|_| format!("invalid month partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid month partition_value: {partition_value}"))?,
        "year" => NaiveDate::parse_from_str(&format!("{partition_value}0101"), "%Y%m%d")
            .map_err(|_| format!("invalid year partition_value: {partition_value}"))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("invalid year partition_value: {partition_value}"))?,
        _ => return Err(format!("unsupported partition_unit: {partition_unit}")),
    };
    let upper_bound = match partition_unit {
        "hour" => lower_bound + Duration::hours(1),
        "day" => lower_bound + Duration::days(1),
        "month" => add_months(lower_bound, 1)?,
        "year" => add_months(lower_bound, 12)?,
        _ => return Err(format!("unsupported partition_unit: {partition_unit}")),
    };
    Ok(PartitionBounds {
        value: partition_value.to_string(),
        lower_bound,
        upper_bound,
    })
}

pub(in crate::catalog) fn add_months(
    value: NaiveDateTime,
    months: u32,
) -> Result<NaiveDateTime, String> {
    let total_month = value.year() * 12 + value.month0() as i32 + months as i32;
    let year = total_month.div_euclid(12);
    let month0 = total_month.rem_euclid(12) as u32;
    NaiveDate::from_ymd_opt(year, month0 + 1, value.day())
        .and_then(|date| date.and_hms_opt(value.hour(), value.minute(), value.second()))
        .ok_or_else(|| format!("invalid month arithmetic for {value}"))
}

use super::*;

pub(super) fn rewrite_show_partitions_sql(sql: &str) -> Option<String> {
    let (schema, table) = parse_show_partitions_target(sql)?;
    let schema = sql_string_literal(&schema.to_ascii_lowercase());
    let table = sql_string_literal(&table.to_ascii_lowercase());

    Some(format!(
        "
    SELECT
        p.partition_value AS partition
    FROM rsduck_catalog.rs_partition p
    JOIN rsduck_catalog.pg_class c
      ON c.oid = p.parent_relid
    JOIN rsduck_catalog.pg_namespace n
      ON n.oid = c.relnamespace
    WHERE p.parent_relid = c.oid
      AND LOWER(n.nspname) = '{schema}'
      AND LOWER(c.relname) = '{table}'
      AND c.relkind = 'p'
    ORDER BY
      CASE WHEN p.is_null_partition THEN 1 ELSE 0 END,
      p.partition_value
    "
    ))
}

pub(super) fn parse_show_partitions_target(sql: &str) -> Option<(String, String)> {
    let sql = sql.trim();
    let mut idx = 0_usize;

    idx = skip_ascii_ws(sql, idx);
    if !keyword_at(sql, idx, "show") {
        return None;
    }
    idx = skip_ascii_ws(sql, idx + 4);

    if !keyword_at(sql, idx, "partitions") {
        return None;
    }
    idx = skip_ascii_ws(sql, idx + 10);

    let mut parts = Vec::new();
    loop {
        let (part, next_idx) = parse_identifier_part(sql, idx)?;
        parts.push(part);
        idx = skip_ascii_ws(sql, next_idx);

        if sql.as_bytes().get(idx) == Some(&b'.') {
            idx = skip_ascii_ws(sql, idx + 1);
            continue;
        }

        break;
    }

    let rest = sql[idx..].trim();
    if !rest.is_empty() && !rest.chars().all(|ch| ch.is_whitespace() || ch == ';') {
        return None;
    }

    let (schema, table) = match parts.len() {
        1 => ("main".to_string(), parts[0].clone()),
        2 => (parts[0].clone(), parts[1].clone()),
        _ => return None,
    };

    Some((schema, table))
}

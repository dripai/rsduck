use super::*;

pub(in crate::catalog) fn comment_object(
    conn: &Connection,
    object_type: CommentObject,
    object_name: &ObjectName,
    comment: Option<&str>,
    if_exists: bool,
    sql: &str,
) -> Result<usize, String> {
    run_catalog_tx(conn, || {
        let (objoid, classoid, objsubid) = match object_type {
            CommentObject::Schema => {
                let schema = single_name_part(object_name)?;
                reject_reserved_schema(&schema)?;
                match namespace_oid(conn, &schema) {
                    Ok(oid) => (oid, OBJECT_SCHEMA_KIND, 0),
                    Err(_) if if_exists => return Ok(0),
                    Err(err) => return Err(err),
                }
            }
            CommentObject::Table | CommentObject::View | CommentObject::Index => {
                let (schema, relname) = relation_name(object_name)?;
                reject_reserved_schema(&schema)?;
                match find_relation_meta(conn, &schema, &relname)? {
                    Some(meta) => (meta.oid, OBJECT_RELATION_KIND, 0),
                    None if if_exists => return Ok(0),
                    None => return Err(format!("relation does not exist: {schema}.{relname}")),
                }
            }
            CommentObject::Column => {
                let (schema, relname, column) = column_comment_target(object_name)?;
                reject_reserved_schema(&schema)?;
                let Some(meta) = find_relation_meta(conn, &schema, &relname)? else {
                    if if_exists {
                        return Ok(0);
                    }
                    return Err(format!("relation does not exist: {schema}.{relname}"));
                };
                let Some(attnum) = column_attnum(conn, meta.oid, &column)? else {
                    if if_exists {
                        return Ok(0);
                    }
                    return Err(format!(
                        "column does not exist: {schema}.{relname}.{column}"
                    ));
                };
                (meta.oid, OBJECT_RELATION_KIND, attnum)
            }
            _ => return Err(format!("COMMENT ON {object_type} is not supported")),
        };

        let journal_id = insert_journal(conn, "comment_object", objoid, sql)?;
        conn.execute(
            &format!(
                "DELETE FROM rsduck_catalog.rs_comment \
                 WHERE objoid = {objoid} AND classoid = {classoid} AND objsubid = {objsubid}"
            ),
            [],
        )
        .map_err(|e| format!("delete previous comment failed: {e}"))?;
        if let Some(comment) = comment {
            conn.execute(
                &format!(
                    "INSERT INTO rsduck_catalog.rs_comment(objoid, classoid, objsubid, description) \
                     VALUES ({objoid}, {classoid}, {objsubid}, '{}')",
                    sql_string(comment)
                ),
                [],
            )
            .map_err(|e| format!("write object comment failed: {e}"))?;
        }
        finish_journal(conn, journal_id)?;
        Ok(0)
    })
}

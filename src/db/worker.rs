use super::*;
use crate::auth::BlockingAuthenticator;
use std::sync::{Arc, MutexGuard};

pub(super) async fn send_typed_sql(
    tx: &SyncSender<SqlCommand>,
    username: String,
    sql: String,
    route: SqlRoute,
    command: String,
    queue_name: &str,
) -> DbResult<SqlTypedResult> {
    let (resp_tx, resp_rx) = oneshot::channel();
    match tx.try_send(SqlCommand::RunTyped {
        username,
        sql,
        route,
        command,
        resp: resp_tx,
    }) {
        Ok(()) => resp_rx
            .await
            .unwrap_or_else(|_| Err(DbError::worker_stopped(queue_name))),
        Err(TrySendError::Full(_)) => Err(DbError::queue_full(queue_name)),
        Err(TrySendError::Disconnected(_)) => Err(DbError::worker_stopped(queue_name)),
    }
}

pub(super) fn spawn_sql_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SqlCommand>,
    max_result_rows: usize,
    write_gate: Option<Arc<Mutex<()>>>,
) -> JoinHandle<()>
where
    N: Into<String>,
{
    let name = name.into();
    let thread_log_name = name.clone();
    thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            info!("DuckDB worker started: {thread_log_name}");
            while let Ok(command) = rx.recv() {
                match command {
                    SqlCommand::RunTyped {
                        username,
                        sql,
                        route,
                        command,
                        resp,
                    } => {
                        let result = if route == SqlRoute::Write {
                            let _write_guard = lock_snapshot_gate(
                                write_gate
                                    .as_ref()
                                    .expect("write workers require a snapshot gate"),
                            );
                            catch_unwind(AssertUnwindSafe(|| {
                                execute_typed_sql_blocking(
                                    &conn,
                                    &username,
                                    &sql,
                                    route,
                                    &command,
                                    max_result_rows,
                                )
                            }))
                        } else {
                            catch_unwind(AssertUnwindSafe(|| {
                                execute_typed_sql_blocking(
                                    &conn,
                                    &username,
                                    &sql,
                                    route,
                                    &command,
                                    max_result_rows,
                                )
                            }))
                        }
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result.map_err(DbError::execution));
                    }
                    SqlCommand::Authenticate { request, resp } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            crate::catalog::CatalogAuthenticator.authenticate(&conn, &request)
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result.map_err(DbError::execution));
                    }
                    SqlCommand::Describe {
                        username,
                        sql,
                        route,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            describe_sql_blocking(&conn, &username, &sql, route)
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result.map_err(DbError::execution));
                    }
                    SqlCommand::ImportParquet {
                        username,
                        schema,
                        sources,
                        resp,
                    } => {
                        let _write_guard = lock_snapshot_gate(
                            write_gate
                                .as_ref()
                                .expect("write workers require a snapshot gate"),
                        );
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            prepare_snapshot_parquet_extension(&conn, None)?;
                            crate::catalog::import_parquet_tables_as(
                                &conn, &username, &schema, &sources,
                            )
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result.map_err(DbError::execution));
                    }
                    SqlCommand::Shutdown => break,
                }
            }
            info!("DuckDB worker stopped: {thread_log_name}");
        })
        .unwrap_or_else(|e| panic!("spawn DuckDB worker {name} failed: {e}"))
}

fn lock_snapshot_gate(write_gate: &Arc<Mutex<()>>) -> MutexGuard<'_, ()> {
    match write_gate.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!("snapshot gate was poisoned; recovering lock");
            poisoned.into_inner()
        }
    }
}

pub(super) fn spawn_snapshot_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SnapshotCommand>,
    write_gate: Arc<Mutex<()>>,
) -> JoinHandle<()>
where
    N: Into<String>,
{
    let name = name.into();
    let thread_log_name = name.clone();
    thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            info!("DuckDB snapshot worker started: {thread_log_name}");
            while let Ok(command) = rx.recv() {
                match command {
                    SnapshotCommand::Save {
                        username,
                        dir,
                        prefix,
                        resp,
                    } => {
                        let _write_guard = lock_snapshot_gate(&write_gate);
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            if let Some(username) = username.as_deref() {
                                crate::catalog::authorize_snapshot(&conn, username)?;
                            }
                            save_snapshot_blocking(&conn, &dir, &prefix)
                        }))
                        .unwrap_or_else(|e| Err(format!("snapshot worker panicked: {e:?}")));
                        let result = result.map_err(DbError::snapshot);
                        match &result {
                            Ok(path) => info!(
                                target: "rsduck_audit",
                                event = "snapshot_save",
                                username = username.as_deref().unwrap_or("system"),
                                path = path.as_str()
                            ),
                            Err(error) => error!(
                                target: "rsduck_audit",
                                event = "snapshot_save_failed",
                                username = username.as_deref().unwrap_or("system"),
                                error = error.as_str()
                            ),
                        }
                        let _ = resp.send(result);
                    }
                    SnapshotCommand::Shutdown => break,
                }
            }
            info!("DuckDB snapshot worker stopped: {thread_log_name}");
        })
        .unwrap_or_else(|e| panic!("spawn DuckDB snapshot worker {name} failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::lock_snapshot_gate;
    use std::sync::{Arc, Mutex};

    #[test]
    fn snapshot_gate_lock_recovers_from_poison() {
        let gate = Arc::new(Mutex::new(()));
        let thread_gate = gate.clone();
        let _ = std::thread::spawn(move || {
            let _guard = thread_gate.lock().unwrap();
            panic!("poison snapshot gate");
        })
        .join();

        let _guard = lock_snapshot_gate(&gate);
    }
}

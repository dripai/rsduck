async fn send_typed_sql(
    tx: &SyncSender<SqlCommand>,
    username: String,
    sql: String,
    route: SqlRoute,
    command: String,
    queue_name: &str,
) -> Result<SqlTypedResult, String> {
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
            .unwrap_or_else(|_| Err(format!("{queue_name} worker stopped"))),
        Err(TrySendError::Full(_)) => Err(format!("{queue_name} queue is full")),
        Err(TrySendError::Disconnected(_)) => Err(format!("{queue_name} worker stopped")),
    }
}

fn spawn_sql_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SqlCommand>,
    max_result_rows: usize,
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
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            execute_typed_sql_blocking(
                                &conn,
                                &username,
                                &sql,
                                route,
                                &command,
                                max_result_rows,
                            )
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result);
                    }
                    SqlCommand::Authenticate {
                        username,
                        password,
                        resp,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            crate::catalog::authenticate_user(&conn, &username, &password)
                                .map(|_| ())
                        }))
                        .unwrap_or_else(|e| Err(format!("duckdb worker panicked: {e:?}")));
                        let _ = resp.send(result);
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
                        let _ = resp.send(result);
                    }
                    SqlCommand::Shutdown => break,
                }
            }
            info!("DuckDB worker stopped: {thread_log_name}");
        })
        .unwrap_or_else(|e| panic!("spawn DuckDB worker {name} failed: {e}"))
}

fn spawn_snapshot_worker<N>(
    name: N,
    conn: Connection,
    rx: Receiver<SnapshotCommand>,
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
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            if let Some(username) = username.as_deref() {
                                crate::catalog::authorize_snapshot(&conn, username)?;
                            }
                            save_snapshot_blocking(&conn, &dir, &prefix)
                        }))
                        .unwrap_or_else(|e| Err(format!("snapshot worker panicked: {e:?}")));
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

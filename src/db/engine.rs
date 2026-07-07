pub fn init_db(snapshot_dir: Option<&str>, cfg: &DbConfig) {
    let base_conn = Connection::open_in_memory().expect("open in-memory duckdb failed");
    restore_or_initialize(&base_conn, snapshot_dir, &cfg.init_sql)
        .unwrap_or_else(|e| panic!("initialize DuckDB failed: {e}"));

    let read_workers = cfg.read_workers.max(1);
    let max_result_rows = cfg.max_result_rows.max(1);
    let mut read_txs = Vec::with_capacity(read_workers);
    let mut workers = Vec::with_capacity(read_workers + 2);

    let write_conn = base_conn
        .try_clone()
        .expect("clone write connection failed");
    let (write_tx, write_rx) = sync_channel(cfg.write_queue_size.max(1));
    workers.push(spawn_sql_worker(
        "duckdb-write",
        write_conn,
        write_rx,
        max_result_rows,
    ));

    for idx in 0..read_workers {
        let read_conn = base_conn
            .try_clone()
            .unwrap_or_else(|e| panic!("clone read connection {idx} failed: {e}"));
        let (read_tx, read_rx) = sync_channel(cfg.read_queue_size.max(1));
        workers.push(spawn_sql_worker(
            format!("duckdb-read-{idx}"),
            read_conn,
            read_rx,
            max_result_rows,
        ));
        read_txs.push(read_tx);
    }

    let snapshot_conn = base_conn
        .try_clone()
        .expect("clone snapshot connection failed");
    let (snapshot_tx, snapshot_rx) = sync_channel(cfg.snapshot_queue_size.max(1));
    workers.push(spawn_snapshot_worker(
        "duckdb-snapshot",
        snapshot_conn,
        snapshot_rx,
    ));

    let engine = DbEngine {
        read_txs,
        write_tx,
        snapshot_tx,
        next_read: AtomicUsize::new(0),
        _base_conn: Mutex::new(base_conn),
        workers: Mutex::new(workers),
    };

    DB_ENGINE
        .set(engine)
        .unwrap_or_else(|_| panic!("db initialized twice"));
}

pub async fn execute_sql_as(username: String, sql: String) -> Result<SqlResult, String> {
    let sql_trimmed = sql.trim().to_string();
    if sql_trimmed.is_empty() {
        return Err("empty sql".into());
    }

    if crate::catalog::looks_like_privilege_function(&sql_trimmed) {
        return engine()
            .evaluate_privilege_function(username, sql_trimmed)
            .await;
    }

    let decision = route_sql(&sql_trimmed)?;
    match decision.route {
        SqlRoute::Read => {
            engine()
                .query(username, sql_trimmed, decision.route, decision.command)
                .await
        }
        SqlRoute::Write => {
            engine()
                .execute(username, sql_trimmed, decision.route, decision.command)
                .await
        }
    }
}

pub async fn save_snapshot(snapshot_dir: &str, snapshot_prefix: &str) -> Result<String, String> {
    engine()
        .save_snapshot(None, snapshot_dir.to_string(), snapshot_prefix.to_string())
        .await
}

pub async fn save_snapshot_as(
    username: String,
    snapshot_dir: &str,
    snapshot_prefix: &str,
) -> Result<String, String> {
    engine()
        .save_snapshot(
            Some(username),
            snapshot_dir.to_string(),
            snapshot_prefix.to_string(),
        )
        .await
}

pub async fn authenticate_user(username: String, password: String) -> Result<(), String> {
    engine().authenticate(username, password).await
}

pub async fn run_partition_maintenance() -> Result<SqlResult, String> {
    engine()
        .execute(
            "admin".to_string(),
            "CALL rsduck_run_partition_maintenance()".to_string(),
            SqlRoute::Write,
            "CALL".to_string(),
        )
        .await
}

pub fn shutdown_workers() {
    if let Some(engine) = DB_ENGINE.get() {
        engine.shutdown();
    }
}

fn engine() -> &'static DbEngine {
    DB_ENGINE.get().expect("db not initialized")
}

impl DbEngine {
    async fn query(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> Result<SqlResult, String> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        send_sql(&self.read_txs[idx], username, sql, route, command, "read").await
    }

    async fn execute(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> Result<SqlResult, String> {
        send_sql(&self.write_tx, username, sql, route, command, "write").await
    }

    async fn save_snapshot(
        &self,
        username: Option<String>,
        dir: String,
        prefix: String,
    ) -> Result<String, String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.snapshot_tx.try_send(SnapshotCommand::Save {
            username,
            dir,
            prefix,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("snapshot worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("snapshot queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("snapshot worker stopped".into()),
        }
    }

    async fn authenticate(&self, username: String, password: String) -> Result<(), String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.write_tx.try_send(SqlCommand::Authenticate {
            username,
            password,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("write worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("write queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("write worker stopped".into()),
        }
    }

    async fn evaluate_privilege_function(
        &self,
        username: String,
        sql: String,
    ) -> Result<SqlResult, String> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.read_txs[idx].try_send(SqlCommand::PrivilegeFunction {
            username,
            sql,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err("read worker stopped".into())),
            Err(TrySendError::Full(_)) => Err("read queue is full".into()),
            Err(TrySendError::Disconnected(_)) => Err("read worker stopped".into()),
        }
    }

    fn shutdown(&self) {
        let _ = self.write_tx.try_send(SqlCommand::Shutdown);
        for read_tx in &self.read_txs {
            let _ = read_tx.try_send(SqlCommand::Shutdown);
        }
        let _ = self.snapshot_tx.try_send(SnapshotCommand::Shutdown);

        if let Ok(mut workers) = self.workers.lock() {
            while let Some(worker) = workers.pop() {
                if let Err(e) = worker.join() {
                    error!("DuckDB worker thread join failed: {:?}", e);
                }
            }
        }
    }
}


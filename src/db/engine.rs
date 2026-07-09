use super::*;
use std::sync::Arc;

impl DbHandle {
    pub fn open(snapshot_dir: Option<&str>, cfg: &DbConfig) -> Self {
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

        Self {
            engine: Arc::new(DbEngine {
                read_txs,
                write_tx,
                snapshot_tx,
                next_read: AtomicUsize::new(0),
                _base_conn: Mutex::new(base_conn),
                workers: Mutex::new(workers),
            }),
        }
    }

    pub async fn execute_typed_sql_as(
        &self,
        username: String,
        sql: String,
    ) -> DbResult<SqlTypedResult> {
        self.execute_typed_sql_with_params_as(username, sql, Vec::new())
            .await
    }

    pub async fn execute_typed_sql_with_params_as(
        &self,
        username: String,
        sql: String,
        params: Vec<SqlParam>,
    ) -> DbResult<SqlTypedResult> {
        let sql_trimmed = sql.trim().to_string();
        if sql_trimmed.is_empty() {
            return Err(DbError::invalid_input("empty sql"));
        }

        let sql_bound = bind_sql_params(&sql_trimmed, &params).map_err(DbError::invalid_input)?;
        let decision = route_sql(&sql_bound).map_err(DbError::invalid_input)?;
        match decision.route {
            SqlRoute::Read => {
                self.engine
                    .query_typed(username, sql_bound, decision.route, decision.command)
                    .await
            }
            SqlRoute::Write => {
                self.engine
                    .execute_typed(username, sql_bound, decision.route, decision.command)
                    .await
            }
        }
    }

    pub async fn describe_sql_with_params_as(
        &self,
        username: String,
        sql: String,
        params: Vec<SqlParam>,
    ) -> DbResult<Vec<SqlColumn>> {
        let sql_trimmed = sql.trim().to_string();
        if sql_trimmed.is_empty() {
            return Err(DbError::invalid_input("empty sql"));
        }

        let sql_bound = bind_sql_params(&sql_trimmed, &params).map_err(DbError::invalid_input)?;
        let decision = route_sql(&sql_bound).map_err(DbError::invalid_input)?;
        self.engine
            .describe(username, sql_bound, decision.route)
            .await
    }

    pub async fn save_snapshot(
        &self,
        snapshot_dir: &str,
        snapshot_prefix: &str,
    ) -> DbResult<String> {
        self.engine
            .save_snapshot(None, snapshot_dir.to_string(), snapshot_prefix.to_string())
            .await
    }

    pub async fn save_snapshot_as(
        &self,
        username: String,
        snapshot_dir: &str,
        snapshot_prefix: &str,
    ) -> DbResult<String> {
        self.engine
            .save_snapshot(
                Some(username),
                snapshot_dir.to_string(),
                snapshot_prefix.to_string(),
            )
            .await
    }

    pub async fn authenticate_user(&self, username: String, password: String) -> DbResult<()> {
        self.engine.authenticate(username, password).await
    }

    pub async fn run_partition_maintenance(&self) -> DbResult<SqlResult> {
        self.engine
            .execute_typed(
                "admin".to_string(),
                "CALL rsduck_run_partition_maintenance()".to_string(),
                SqlRoute::Write,
                "CALL".to_string(),
            )
            .await
            .map(SqlResult::from)
    }

    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}

impl DbEngine {
    async fn query_typed(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> DbResult<SqlTypedResult> {
        let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
        send_typed_sql(&self.read_txs[idx], username, sql, route, command, "read").await
    }

    async fn execute_typed(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
        command: String,
    ) -> DbResult<SqlTypedResult> {
        send_typed_sql(&self.write_tx, username, sql, route, command, "write").await
    }

    async fn save_snapshot(
        &self,
        username: Option<String>,
        dir: String,
        prefix: String,
    ) -> DbResult<String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.snapshot_tx.try_send(SnapshotCommand::Save {
            username,
            dir,
            prefix,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err(DbError::worker_stopped("snapshot"))),
            Err(TrySendError::Full(_)) => Err(DbError::queue_full("snapshot")),
            Err(TrySendError::Disconnected(_)) => Err(DbError::worker_stopped("snapshot")),
        }
    }

    async fn authenticate(&self, username: String, password: String) -> DbResult<()> {
        let (resp_tx, resp_rx) = oneshot::channel();
        match self.write_tx.try_send(SqlCommand::Authenticate {
            username,
            password,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err(DbError::worker_stopped("write"))),
            Err(TrySendError::Full(_)) => Err(DbError::queue_full("write")),
            Err(TrySendError::Disconnected(_)) => Err(DbError::worker_stopped("write")),
        }
    }

    async fn describe(
        &self,
        username: String,
        sql: String,
        route: SqlRoute,
    ) -> DbResult<Vec<SqlColumn>> {
        let (tx, queue_name) = match route {
            SqlRoute::Read => {
                let idx = self.next_read.fetch_add(1, Ordering::Relaxed) % self.read_txs.len();
                (&self.read_txs[idx], "read")
            }
            SqlRoute::Write => (&self.write_tx, "write"),
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        match tx.try_send(SqlCommand::Describe {
            username,
            sql,
            route,
            resp: resp_tx,
        }) {
            Ok(()) => resp_rx
                .await
                .unwrap_or_else(|_| Err(DbError::worker_stopped(queue_name))),
            Err(TrySendError::Full(_)) => Err(DbError::queue_full(queue_name)),
            Err(TrySendError::Disconnected(_)) => Err(DbError::worker_stopped(queue_name)),
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

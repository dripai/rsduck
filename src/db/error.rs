use std::fmt;

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DbErrorKind {
    InvalidInput,
    QueueFull,
    WorkerStopped,
    Execution,
    Snapshot,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DbError {
    kind: DbErrorKind,
    message: String,
}

impl DbError {
    pub fn new(kind: DbErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(DbErrorKind::InvalidInput, message)
    }

    pub(super) fn queue_full(queue_name: &str) -> Self {
        Self::new(
            DbErrorKind::QueueFull,
            format!("{queue_name} queue is full"),
        )
    }

    pub(super) fn worker_stopped(queue_name: &str) -> Self {
        Self::new(
            DbErrorKind::WorkerStopped,
            format!("{queue_name} worker stopped"),
        )
    }

    pub(super) fn execution(message: impl Into<String>) -> Self {
        Self::new(DbErrorKind::Execution, message)
    }

    pub(super) fn snapshot(message: impl Into<String>) -> Self {
        Self::new(DbErrorKind::Snapshot, message)
    }

    pub fn kind(&self) -> DbErrorKind {
        self.kind
    }

    pub fn as_str(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

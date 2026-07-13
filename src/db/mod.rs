use crate::config::DbConfig;
use crate::sql_route::{route_sql, SqlRoute};
use duckdb::{types::ValueRef, Connection};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Instant;
use tokio::sync::oneshot;
use tracing::{error, info};

mod engine;
mod error;
mod execute;
mod extensions;
mod model;
mod params;
mod restore;
mod snapshot;
mod vector;
mod worker;

pub use self::error::*;
#[allow(unused_imports)]
pub(crate) use self::execute::*;
pub use self::extensions::*;
pub use self::model::*;
pub use self::params::*;
#[allow(unused_imports)]
pub(crate) use self::restore::*;
pub use self::snapshot::*;
pub(crate) use self::vector::*;
#[allow(unused_imports)]
pub(crate) use self::worker::*;

#[cfg(test)]
mod tests;

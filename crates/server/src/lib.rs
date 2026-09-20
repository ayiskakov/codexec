//! codexec-server: HTTP API, durable submission queue and judge workers in
//! one process: the "single box" deployment from the design doc. The seams
//! that the cluster version swaps out are `Store` (Postgres), `RunStore` and
//! `Hub` (Redis / NATS), and the in-process workers (separate worker VMs).

pub mod config;
pub mod events;
pub mod http;
pub mod runs;
pub mod store;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use codexec_judge::{Judge, ProblemSet};
use tokio::sync::Notify;

use crate::config::ServerConfig;
use crate::events::Hub;
use crate::runs::RunStore;
use crate::store::Store;

pub struct AppState {
    pub config: ServerConfig,
    pub store: Store,
    pub runs: RunStore,
    pub hub: Arc<Hub>,
    pub judge: Judge,
    pub problems: ProblemSet,
    /// Wakes idle workers when a job is enqueued.
    pub wake: Notify,
}

impl AppState {
    pub fn new(config: ServerConfig, store: Store, judge: Judge, problems: ProblemSet) -> Arc<Self> {
        let runs = RunStore::new(config.run_queue_capacity, Duration::from_secs(config.run_ttl_seconds));
        Arc::new(Self {
            config,
            store,
            runs,
            hub: Arc::new(Hub::default()),
            judge,
            problems,
            wake: Notify::new(),
        })
    }

    pub fn lease_ms(&self) -> i64 {
        (self.config.lease_seconds * 1000) as i64
    }
}

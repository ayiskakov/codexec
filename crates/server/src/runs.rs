//! The Run lane: interactive executions against samples or custom input.
//!
//! Runs are deliberately not durable (the design doc keeps them in Redis with
//! a TTL). Here they live in memory: a bounded queue plus a result map that
//! the sweeper prunes.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use codexec_judge::{Comparer, JudgeResult, TestCase};
use serde::Serialize;
use uuid::Uuid;

use crate::store::{now_ms, Status};

pub struct RunJob {
    pub id: String,
    pub language: String,
    pub source: String,
    pub tests: Vec<TestCase>,
    pub time_limit_ms: u64,
    pub memory_limit_kb: u64,
    pub comparer: Comparer,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub id: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JudgeResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: i64,
    #[serde(skip)]
    created: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub struct QueueFull;

pub struct RunStore {
    capacity: usize,
    ttl: Duration,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<RunJob>,
    records: HashMap<String, RunRecord>,
}

impl RunStore {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self { capacity, ttl, inner: Mutex::new(Inner::default()) }
    }

    /// Enqueues a run; refuses when the lane is full so callers can return 503.
    pub fn submit(&self, mut job: RunJob) -> Result<RunRecord, QueueFull> {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= self.capacity {
            return Err(QueueFull);
        }
        job.id = Uuid::now_v7().to_string();
        let record = RunRecord {
            id: job.id.clone(),
            status: Status::Queued,
            result: None,
            error: None,
            created_at_ms: now_ms(),
            created: Instant::now(),
        };
        inner.records.insert(job.id.clone(), record.clone());
        inner.queue.push_back(job);
        Ok(record)
    }

    pub fn pop(&self) -> Option<RunJob> {
        let mut inner = self.inner.lock().unwrap();
        let job = inner.queue.pop_front()?;
        if let Some(rec) = inner.records.get_mut(&job.id) {
            rec.status = Status::Running;
        }
        Some(job)
    }

    pub fn complete(&self, id: &str, outcome: Result<JudgeResult, String>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(rec) = inner.records.get_mut(id) {
            rec.status = Status::Done;
            match outcome {
                Ok(result) => rec.result = Some(result),
                Err(error) => rec.error = Some(error),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<RunRecord> {
        self.inner.lock().unwrap().records.get(id).cloned()
    }

    pub fn queue_depth(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    /// Drops finished records older than the TTL.
    pub fn prune(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.records.len();
        let ttl = self.ttl;
        inner.records.retain(|_, r| r.status != Status::Done || r.created.elapsed() < ttl);
        before - inner.records.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> RunJob {
        RunJob {
            id: String::new(),
            language: "python".into(),
            source: "print(1)".into(),
            tests: vec![],
            time_limit_ms: 1000,
            memory_limit_kb: 1024,
            comparer: Comparer::Lines,
        }
    }

    #[test]
    fn bounded_fifo_with_ttl() {
        let runs = RunStore::new(2, Duration::from_millis(0));
        let a = runs.submit(job()).unwrap();
        let b = runs.submit(job()).unwrap();
        assert_eq!(runs.submit(job()).unwrap_err(), QueueFull);

        assert_eq!(runs.pop().unwrap().id, a.id);
        assert_eq!(runs.get(&a.id).unwrap().status, Status::Running);
        runs.complete(&a.id, Err("sandbox down".into()));
        assert_eq!(runs.get(&a.id).unwrap().error.as_deref(), Some("sandbox down"));

        assert_eq!(runs.prune(), 1, "finished and past TTL");
        assert!(runs.get(&a.id).is_none());
        assert!(runs.get(&b.id).is_some(), "queued runs are never pruned");
    }
}

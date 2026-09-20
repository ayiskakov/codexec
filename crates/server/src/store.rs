//! SQLite store. The `submissions` table is also the durable job queue, the
//! single-box stand-in for Postgres `FOR UPDATE SKIP LOCKED`.
//!
//! Delivery is at-least-once. Three things keep that safe:
//! * judging is a pure function, so a duplicate run produces the same verdict;
//! * every write from a worker is fenced by `attempt`, so a worker that lost
//!   its lease cannot overwrite the result of the worker that took over;
//! * the sweeper re-queues jobs whose lease expired and gives up with IE
//!   after `max_attempts`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use codexec_judge::{JudgeResult, TestResult, Verdict};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use uuid::Uuid;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS submissions (
    id               TEXT PRIMARY KEY,           -- UUIDv7: time-ordered
    user_id          TEXT NOT NULL,
    problem          TEXT NOT NULL,
    problem_version  TEXT NOT NULL,
    language         TEXT NOT NULL,
    status           TEXT NOT NULL,              -- QUEUED | COMPILING | RUNNING | DONE
    verdict          TEXT,
    time_ms          INTEGER,
    memory_kb        INTEGER,
    failed_test      TEXT,
    tests_done       INTEGER NOT NULL DEFAULT 0,
    tests_total      INTEGER NOT NULL DEFAULT 0,
    test_results     TEXT,                       -- JSON array, one row per submission, not per test
    compile_output   TEXT,
    error            TEXT,                       -- last infrastructure error, for IE triage
    judge_version    TEXT,
    attempt          INTEGER NOT NULL DEFAULT 0,
    lease_until_ms   INTEGER,
    idempotency_key  TEXT UNIQUE,
    created_at_ms    INTEGER NOT NULL,
    judged_at_ms     INTEGER
);

-- Kept out of the hot table so listing and queue scans stay narrow.
CREATE TABLE IF NOT EXISTS submission_sources (
    submission_id TEXT PRIMARY KEY REFERENCES submissions(id) ON DELETE CASCADE,
    source        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_submissions_queue ON submissions(created_at_ms) WHERE status = 'QUEUED';
CREATE INDEX IF NOT EXISTS idx_submissions_inflight ON submissions(lease_until_ms) WHERE status IN ('COMPILING', 'RUNNING');
CREATE INDEX IF NOT EXISTS idx_submissions_user ON submissions(user_id, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_submissions_problem ON submissions(problem, created_at_ms DESC);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Queued,
    Compiling,
    Running,
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "QUEUED",
            Status::Compiling => "COMPILING",
            Status::Running => "RUNNING",
            Status::Done => "DONE",
        }
    }

    fn parse(s: &str) -> Status {
        match s {
            "COMPILING" => Status::Compiling,
            "RUNNING" => Status::Running,
            "DONE" => Status::Done,
            _ => Status::Queued,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Submission {
    pub id: String,
    pub problem: String,
    pub problem_version: String,
    pub language: String,
    pub status: Status,
    pub verdict: Option<Verdict>,
    pub time_ms: Option<u64>,
    pub memory_kb: Option<u64>,
    pub failed_test: Option<String>,
    pub tests_done: u64,
    pub tests_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<Vec<TestResult>>,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_version: Option<String>,
    pub created_at_ms: i64,
    pub judged_at_ms: Option<i64>,
}

pub struct NewSubmission {
    pub user_id: String,
    pub problem: String,
    pub problem_version: String,
    pub language: String,
    pub source: String,
    pub tests_total: u64,
    pub idempotency_key: Option<String>,
}

/// A claimed job. `attempt` is the fencing token for every later write.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub problem: String,
    pub language: String,
    pub source: String,
    pub attempt: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub requeued: usize,
    pub failed: usize,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const COLUMNS: &str = "id, problem, problem_version, language, status, verdict, time_ms, memory_kb, \
    failed_test, tests_done, tests_total, compile_output, test_results, attempt, judge_version, \
    created_at_ms, judged_at_ms";

fn row_to_submission(row: &Row<'_>) -> rusqlite::Result<Submission> {
    let status: String = row.get(4)?;
    let verdict: Option<String> = row.get(5)?;
    let tests_json: Option<String> = row.get(12)?;
    Ok(Submission {
        id: row.get(0)?,
        problem: row.get(1)?,
        problem_version: row.get(2)?,
        language: row.get(3)?,
        status: Status::parse(&status),
        verdict: verdict.as_deref().and_then(Verdict::from_code),
        time_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        memory_kb: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        failed_test: row.get(8)?,
        tests_done: row.get::<_, i64>(9)? as u64,
        tests_total: row.get::<_, i64>(10)? as u64,
        compile_output: row.get(11)?,
        tests: tests_json.and_then(|j| serde_json::from_str(&j).ok()),
        attempt: row.get::<_, i64>(13)? as u32,
        judge_version: row.get(14)?,
        created_at_ms: row.get(15)?,
        judged_at_ms: row.get(16)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Runs blocking SQLite work off the async executor.
    async fn with<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
            f(&mut guard)
        })
        .await
        .context("store task panicked")?
    }

    /// Inserts a submission in state QUEUED. With an idempotency key, a repeat
    /// returns the original row and `false`.
    pub async fn insert(&self, new: NewSubmission) -> Result<(Submission, bool)> {
        self.with(move |conn| {
            let tx = conn.transaction()?;
            if let Some(key) = &new.idempotency_key {
                let existing = tx
                    .query_row(
                        &format!("SELECT {COLUMNS} FROM submissions WHERE idempotency_key = ?1"),
                        params![key],
                        row_to_submission,
                    )
                    .optional()?;
                if let Some(sub) = existing {
                    return Ok((sub, false));
                }
            }
            let id = Uuid::now_v7().to_string();
            tx.execute(
                "INSERT INTO submissions (id, user_id, problem, problem_version, language, status, \
                 tests_total, idempotency_key, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'QUEUED', ?6, ?7, ?8)",
                params![
                    id,
                    new.user_id,
                    new.problem,
                    new.problem_version,
                    new.language,
                    new.tests_total as i64,
                    new.idempotency_key,
                    now_ms()
                ],
            )?;
            tx.execute(
                "INSERT INTO submission_sources (submission_id, source) VALUES (?1, ?2)",
                params![id, new.source],
            )?;
            let sub = tx.query_row(
                &format!("SELECT {COLUMNS} FROM submissions WHERE id = ?1"),
                params![id],
                row_to_submission,
            )?;
            tx.commit()?;
            Ok((sub, true))
        })
        .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<Submission>> {
        let id = id.to_string();
        self.with(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM submissions WHERE id = ?1"),
                    params![id],
                    row_to_submission,
                )
                .optional()?)
        })
        .await
    }

    pub async fn get_source(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        self.with(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT source FROM submission_sources WHERE submission_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    /// Newest first. Per-test details are omitted from listings.
    pub async fn list(&self, problem: Option<String>, limit: u32) -> Result<Vec<Submission>> {
        self.with(move |conn| {
            let sql = format!(
                "SELECT {COLUMNS} FROM submissions WHERE (?1 IS NULL OR problem = ?1) \
                 ORDER BY created_at_ms DESC, id DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![problem, limit], row_to_submission)?;
            let mut out = Vec::new();
            for row in rows {
                let mut sub = row?;
                sub.tests = None;
                sub.compile_output = None;
                out.push(sub);
            }
            Ok(out)
        })
        .await
    }

    /// Atomically takes the oldest queued submission and leases it.
    pub async fn claim_next(&self, lease_ms: i64) -> Result<Option<Job>> {
        self.with(move |conn| {
            let tx = conn.transaction()?;
            let claimed = tx
                .query_row(
                    "UPDATE submissions \
                     SET status = 'COMPILING', attempt = attempt + 1, lease_until_ms = ?1, tests_done = 0 \
                     WHERE id = (SELECT id FROM submissions WHERE status = 'QUEUED' \
                                 ORDER BY created_at_ms, id LIMIT 1) \
                     RETURNING id, problem, language, attempt",
                    params![now_ms() + lease_ms],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)? as u32,
                        ))
                    },
                )
                .optional()?;
            let Some((id, problem, language, attempt)) = claimed else {
                return Ok(None);
            };
            let source: String = tx.query_row(
                "SELECT source FROM submission_sources WHERE submission_id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok(Some(Job { id, problem, language, source, attempt }))
        })
        .await
    }

    /// Heartbeat: records progress and extends the lease. Returns false when
    /// the lease was lost (another attempt owns the job now).
    pub async fn progress(
        &self,
        id: &str,
        attempt: u32,
        status: Status,
        tests_done: u64,
        tests_total: u64,
        lease_ms: i64,
    ) -> Result<bool> {
        let id = id.to_string();
        self.with(move |conn| {
            let n = conn.execute(
                "UPDATE submissions SET status = ?1, tests_done = ?2, tests_total = ?3, lease_until_ms = ?4 \
                 WHERE id = ?5 AND attempt = ?6 AND status IN ('COMPILING', 'RUNNING')",
                params![
                    status.as_str(),
                    tests_done as i64,
                    tests_total as i64,
                    now_ms() + lease_ms,
                    id,
                    attempt
                ],
            )?;
            Ok(n == 1)
        })
        .await
    }

    /// Writes the verdict. Conditional on the attempt and on not being DONE,
    /// so duplicates and stale workers are no-ops. Returns whether it applied.
    pub async fn finish(
        &self,
        id: &str,
        attempt: u32,
        problem_version: &str,
        judge_version: &str,
        result: &JudgeResult,
    ) -> Result<bool> {
        let id = id.to_string();
        let problem_version = problem_version.to_string();
        let judge_version = judge_version.to_string();
        let tests_json = serde_json::to_string(&result.tests)?;
        let verdict = result.verdict.code();
        let (time_ms, memory_kb) = (result.time_ms as i64, result.memory_kb as i64);
        let failed_test = result.failed_test.clone();
        let compile_output = result.compile_output.clone();
        let (done, total) = (result.tests.len() as i64, result.tests_total as i64);
        self.with(move |conn| {
            let n = conn.execute(
                "UPDATE submissions SET status = 'DONE', verdict = ?1, time_ms = ?2, memory_kb = ?3, \
                 failed_test = ?4, test_results = ?5, compile_output = ?6, tests_done = ?7, tests_total = ?8, \
                 problem_version = ?9, judge_version = ?10, judged_at_ms = ?11, lease_until_ms = NULL, error = NULL \
                 WHERE id = ?12 AND attempt = ?13 AND status != 'DONE'",
                params![
                    verdict,
                    time_ms,
                    memory_kb,
                    failed_test,
                    tests_json,
                    compile_output,
                    done,
                    total,
                    problem_version,
                    judge_version,
                    now_ms(),
                    id,
                    attempt
                ],
            )?;
            Ok(n == 1)
        })
        .await
    }

    /// Infrastructure failure: back to the queue, or IE once attempts are used up.
    /// Returns the resulting status, or None if the lease was already lost.
    pub async fn fail_attempt(
        &self,
        id: &str,
        attempt: u32,
        max_attempts: u32,
        error: &str,
    ) -> Result<Option<Status>> {
        let id = id.to_string();
        let error = error.to_string();
        self.with(move |conn| {
            let give_up = attempt >= max_attempts;
            let n = if give_up {
                conn.execute(
                    "UPDATE submissions SET status = 'DONE', verdict = 'IE', error = ?1, judged_at_ms = ?2, \
                     lease_until_ms = NULL WHERE id = ?3 AND attempt = ?4 AND status != 'DONE'",
                    params![error, now_ms(), id, attempt],
                )?
            } else {
                conn.execute(
                    "UPDATE submissions SET status = 'QUEUED', error = ?1, lease_until_ms = NULL \
                     WHERE id = ?2 AND attempt = ?3 AND status != 'DONE'",
                    params![error, id, attempt],
                )?
            };
            Ok((n == 1).then_some(if give_up { Status::Done } else { Status::Queued }))
        })
        .await
    }

    /// Re-queues jobs whose lease expired; marks them IE after `max_attempts`.
    /// This one mechanism covers crashed workers and lost jobs alike.
    pub async fn sweep(&self, max_attempts: u32) -> Result<SweepReport> {
        self.with(move |conn| {
            let tx = conn.transaction()?;
            let now = now_ms();
            let failed = tx.execute(
                "UPDATE submissions SET status = 'DONE', verdict = 'IE', judged_at_ms = ?1, lease_until_ms = NULL, \
                 error = COALESCE(error, 'lease expired') \
                 WHERE status IN ('COMPILING', 'RUNNING') AND lease_until_ms < ?1 AND attempt >= ?2",
                params![now, max_attempts],
            )?;
            let requeued = tx.execute(
                "UPDATE submissions SET status = 'QUEUED', lease_until_ms = NULL \
                 WHERE status IN ('COMPILING', 'RUNNING') AND lease_until_ms < ?1",
                params![now],
            )?;
            tx.commit()?;
            Ok(SweepReport { requeued, failed })
        })
        .await
    }

    /// At start-up this process owns every job, so anything in flight belonged
    /// to a previous run and can be re-queued without waiting for its lease.
    pub async fn requeue_inflight(&self) -> Result<usize> {
        self.with(|conn| {
            Ok(conn.execute(
                "UPDATE submissions SET status = 'QUEUED', lease_until_ms = NULL \
                 WHERE status IN ('COMPILING', 'RUNNING')",
                [],
            )?)
        })
        .await
    }

    pub async fn queue_depth(&self) -> Result<u64> {
        self.with(|conn| {
            let n: i64 =
                conn.query_row("SELECT COUNT(*) FROM submissions WHERE status = 'QUEUED'", [], |r| r.get(0))?;
            Ok(n as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_sub(problem: &str, key: Option<&str>) -> NewSubmission {
        NewSubmission {
            user_id: "local".into(),
            problem: problem.into(),
            problem_version: "v1".into(),
            language: "python".into(),
            source: "print(1)".into(),
            tests_total: 3,
            idempotency_key: key.map(str::to_string),
        }
    }

    fn result(verdict: Verdict) -> JudgeResult {
        JudgeResult {
            verdict,
            time_ms: 12,
            memory_kb: 3456,
            failed_test: None,
            compile_output: None,
            tests: vec![],
            tests_total: 3,
        }
    }

    #[tokio::test]
    async fn queue_is_fifo_and_claims_are_exclusive() {
        let store = Store::open_in_memory().unwrap();
        let (a, _) = store.insert(new_sub("p1", None)).await.unwrap();
        let (b, _) = store.insert(new_sub("p2", None)).await.unwrap();
        assert_eq!(store.queue_depth().await.unwrap(), 2);

        let j1 = store.claim_next(60_000).await.unwrap().unwrap();
        let j2 = store.claim_next(60_000).await.unwrap().unwrap();
        assert_eq!((j1.id.as_str(), j2.id.as_str()), (a.id.as_str(), b.id.as_str()));
        assert_eq!(j1.attempt, 1);
        assert_eq!(j1.source, "print(1)");
        assert!(store.claim_next(60_000).await.unwrap().is_none());
        assert_eq!(store.get(&a.id).await.unwrap().unwrap().status, Status::Compiling);
    }

    #[tokio::test]
    async fn idempotency_key_returns_the_original() {
        let store = Store::open_in_memory().unwrap();
        let (first, created) = store.insert(new_sub("p", Some("k1"))).await.unwrap();
        let (second, created_again) = store.insert(new_sub("p", Some("k1"))).await.unwrap();
        assert!(created && !created_again);
        assert_eq!(first.id, second.id);
        assert_eq!(store.queue_depth().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn finish_is_fenced_by_attempt_and_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let (sub, _) = store.insert(new_sub("p", None)).await.unwrap();
        let job = store.claim_next(60_000).await.unwrap().unwrap();

        assert!(!store
            .finish(&sub.id, job.attempt + 1, "v2", "j", &result(Verdict::WrongAnswer))
            .await
            .unwrap());
        assert!(store.finish(&sub.id, job.attempt, "v2", "j", &result(Verdict::Accepted)).await.unwrap());
        assert!(!store.finish(&sub.id, job.attempt, "v2", "j", &result(Verdict::WrongAnswer)).await.unwrap());

        let got = store.get(&sub.id).await.unwrap().unwrap();
        assert_eq!(got.status, Status::Done);
        assert_eq!(got.verdict, Some(Verdict::Accepted));
        assert_eq!(got.problem_version, "v2");
        assert_eq!(got.time_ms, Some(12));
        assert!(!store.progress(&sub.id, job.attempt, Status::Running, 1, 3, 1000).await.unwrap());
    }

    #[tokio::test]
    async fn expired_leases_are_requeued_then_failed() {
        let store = Store::open_in_memory().unwrap();
        let (sub, _) = store.insert(new_sub("p", None)).await.unwrap();

        for attempt in 1..=2u32 {
            let job = store.claim_next(-1).await.unwrap().unwrap(); // already expired
            assert_eq!(job.attempt, attempt);
            let report = store.sweep(2).await.unwrap();
            if attempt < 2 {
                assert_eq!(report, SweepReport { requeued: 1, failed: 0 });
            } else {
                assert_eq!(report, SweepReport { requeued: 0, failed: 1 });
            }
        }
        let got = store.get(&sub.id).await.unwrap().unwrap();
        assert_eq!((got.status, got.verdict), (Status::Done, Some(Verdict::InternalError)));
    }

    #[tokio::test]
    async fn live_leases_are_left_alone_and_heartbeats_extend_them() {
        let store = Store::open_in_memory().unwrap();
        let (sub, _) = store.insert(new_sub("p", None)).await.unwrap();
        let job = store.claim_next(60_000).await.unwrap().unwrap();
        assert_eq!(store.sweep(3).await.unwrap(), SweepReport::default());
        assert!(store.progress(&sub.id, job.attempt, Status::Running, 2, 3, 60_000).await.unwrap());
        let got = store.get(&sub.id).await.unwrap().unwrap();
        assert_eq!((got.status, got.tests_done), (Status::Running, 2));
    }

    #[tokio::test]
    async fn fail_attempt_requeues_then_gives_up() {
        let store = Store::open_in_memory().unwrap();
        let (sub, _) = store.insert(new_sub("p", None)).await.unwrap();
        let job = store.claim_next(60_000).await.unwrap().unwrap();
        assert_eq!(store.fail_attempt(&sub.id, job.attempt, 2, "boom").await.unwrap(), Some(Status::Queued));
        let job = store.claim_next(60_000).await.unwrap().unwrap();
        assert_eq!(job.attempt, 2);
        assert_eq!(store.fail_attempt(&sub.id, job.attempt, 2, "boom").await.unwrap(), Some(Status::Done));
        assert_eq!(store.get(&sub.id).await.unwrap().unwrap().verdict, Some(Verdict::InternalError));
    }

    #[tokio::test]
    async fn restart_requeues_inflight_and_list_filters() {
        let store = Store::open_in_memory().unwrap();
        store.insert(new_sub("p1", None)).await.unwrap();
        store.insert(new_sub("p2", None)).await.unwrap();
        store.claim_next(60_000).await.unwrap().unwrap();
        assert_eq!(store.requeue_inflight().await.unwrap(), 1);
        assert_eq!(store.queue_depth().await.unwrap(), 2);
        assert_eq!(store.list(Some("p2".into()), 10).await.unwrap().len(), 1);
        assert_eq!(store.list(None, 10).await.unwrap().len(), 2);
        assert_eq!(store.list(None, 1).await.unwrap()[0].problem, "p2", "newest first");
    }
}

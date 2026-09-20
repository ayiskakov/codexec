//! Judge workers and the sweeper.
//!
//! Workers pull; nothing is pushed to them. A worker takes a job only when it
//! is idle, so backpressure is automatic and the queue absorbs bursts. The
//! sandbox pool has exactly as many boxes as there are workers.

use std::sync::Arc;
use std::time::Duration;

use codexec_judge::{JudgeError, JudgeRequest, Progress, JUDGE_VERSION};
use tokio::sync::mpsc::unbounded_channel;

use crate::runs::RunJob;
use crate::store::{Job, Status};
use crate::AppState;

/// Runs served in a row before a waiting submission gets a turn. Weighted
/// rather than strict priority, so neither lane can starve the other.
const RUN_BURST: u32 = 3;

pub fn spawn_workers(state: Arc<AppState>, count: u32) {
    for slot in 0..count {
        let state = state.clone();
        tokio::spawn(async move { worker_loop(state, slot).await });
    }
}

pub fn spawn_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            match state.store.sweep(state.config.max_attempts).await {
                Ok(report) if report.requeued + report.failed > 0 => {
                    tracing::warn!(
                        requeued = report.requeued,
                        failed = report.failed,
                        "sweeper recovered expired leases"
                    );
                    state.wake.notify_waiters();
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "sweep failed"),
            }
            state.runs.prune();
            state.hub.prune();
        }
    });
}

async fn worker_loop(state: Arc<AppState>, slot: u32) {
    let mut run_streak = 0u32;
    loop {
        let prefer_submission = run_streak >= RUN_BURST;
        if !prefer_submission {
            if let Some(job) = state.runs.pop() {
                run_streak += 1;
                handle_run(&state, job).await;
                continue;
            }
        }
        match state.store.claim_next(state.lease_ms()).await {
            Ok(Some(job)) => {
                run_streak = 0;
                handle_submission(&state, slot, job).await;
                continue;
            }
            Ok(None) => run_streak = 0,
            Err(e) => {
                tracing::error!(slot, error = %e, "claim failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        if prefer_submission {
            continue; // no submission was waiting; go back to serving runs
        }
        // Idle. The timeout is a safety net for wake-ups lost between the checks above and here.
        let _ = tokio::time::timeout(Duration::from_millis(500), state.wake.notified()).await;
    }
}

async fn handle_run(state: &AppState, job: RunJob) {
    let req = JudgeRequest {
        language: &job.language,
        source: &job.source,
        tests: &job.tests,
        time_limit_ms: job.time_limit_ms,
        memory_limit_kb: job.memory_limit_kb,
        comparer: job.comparer,
        stop_on_first_failure: false,
        capture_output: true,
        show_failed_input: true,
    };
    let outcome = state.judge.judge(&req, None).await.map_err(|e| e.to_string());
    if let Err(e) = &outcome {
        tracing::error!(run = %job.id, error = %e, "run failed");
    }
    state.runs.complete(&job.id, outcome);
}

async fn handle_submission(state: &AppState, slot: u32, job: Job) {
    let Some(problem) = state.problems.get(&job.problem) else {
        fail_permanently(state, &job, "problem no longer exists").await;
        return;
    };

    let req = JudgeRequest {
        language: &job.language,
        source: &job.source,
        tests: &problem.tests,
        time_limit_ms: problem.time_limit_ms,
        memory_limit_kb: problem.memory_limit_kb,
        comparer: problem.comparer,
        stop_on_first_failure: true,
        capture_output: false,
        show_failed_input: problem.show_failed_input,
    };

    // Progress doubles as the lease heartbeat.
    let (tx, mut rx) = unbounded_channel::<Progress>();
    let heartbeat = {
        let (store, hub, id, attempt, lease) =
            (state.store.clone(), state.hub.clone(), job.id.clone(), job.attempt, state.lease_ms());
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let (status, done, total) = match p {
                    Progress::Compiling => (Status::Compiling, 0, 0),
                    Progress::Running { done, total } => (Status::Running, done as u64, total as u64),
                };
                if let Ok(true) = store.progress(&id, attempt, status, done, total, lease).await {
                    if let Ok(Some(sub)) = store.get(&id).await {
                        hub.publish(&sub);
                    }
                }
            }
        })
    };

    let outcome = state.judge.judge(&req, Some(&tx)).await;
    drop(tx);
    let _ = heartbeat.await;

    match outcome {
        Ok(result) => {
            let applied =
                state.store.finish(&job.id, job.attempt, &problem.version, JUDGE_VERSION, &result).await;
            match applied {
                Ok(true) => tracing::info!(
                    slot, submission = %job.id, problem = %job.problem, language = %job.language,
                    verdict = %result.verdict, time_ms = result.time_ms, memory_kb = result.memory_kb,
                    "judged"
                ),
                Ok(false) => tracing::warn!(submission = %job.id, "result discarded: lease was lost"),
                Err(e) => tracing::error!(submission = %job.id, error = %e, "could not store result"),
            }
        }
        Err(JudgeError::UnknownLanguage(lang)) => {
            // Not retryable: the language was removed after the submission was accepted.
            fail_permanently(state, &job, &format!("language {lang:?} is not configured")).await;
        }
        Err(JudgeError::Sandbox(e)) => fail(state, &job, &e.to_string()).await,
    }

    if let Ok(Some(sub)) = state.store.get(&job.id).await {
        state.hub.publish(&sub);
    }
}

async fn fail(state: &AppState, job: &Job, error: &str) {
    tracing::error!(submission = %job.id, attempt = job.attempt, error, "judging attempt failed");
    match state.store.fail_attempt(&job.id, job.attempt, state.config.max_attempts, error).await {
        Ok(Some(Status::Queued)) => {
            // Brief pause so a broken sandbox does not spin through attempts instantly.
            tokio::time::sleep(Duration::from_millis(500)).await;
            state.wake.notify_one();
        }
        Ok(_) => {}
        Err(e) => tracing::error!(submission = %job.id, error = %e, "could not record failure"),
    }
}

async fn fail_permanently(state: &AppState, job: &Job, error: &str) {
    tracing::error!(submission = %job.id, error, "judging failed permanently");
    if let Err(e) = state.store.fail_attempt(&job.id, job.attempt, 0, error).await {
        tracing::error!(submission = %job.id, error = %e, "could not record failure");
    }
}

//! End-to-end tests of the HTTP API, queue and workers against a scripted
//! sandbox. The real isolate backend is covered by `scripts/e2e.py`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use codexec_judge::mock::MockSandbox;
use codexec_judge::{Judge, JudgeConfig, LanguageRegistry, ProblemSet};
use codexec_server::config::ServerConfig;
use codexec_server::store::Store;
use codexec_server::{http, worker, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const LANGS: &str = r#"
[python]
display_name = "Python"
source_file = "main.py"
[python.run]
argv = ["python3", "main.py"]
"#;

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn problems_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("codexec-api-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let p = root.join("double");
    write(&p, "problem.toml", "title = \"Double\"\nsamples = [\"01\"]\nshow_failed_input = true\n");
    write(&p, "statement.md", "Print 2n.");
    for (name, n) in [("01", 1), ("02", 21), ("03", 500)] {
        write(&p, &format!("tests/{name}.in"), &format!("{n}\n"));
        write(&p, &format!("tests/{name}.out"), &format!("{}\n", n * 2));
    }
    root
}

/// A sandbox whose "program" doubles its input, except that it answers 0 for
/// the input 500 when `buggy` is set.
fn app(tag: &str, buggy: bool, workers: u32) -> Router {
    let sandbox = Arc::new(MockSandbox::new(Box::new(move |_spec, stdin| {
        let n: i64 = String::from_utf8_lossy(stdin).trim().parse().unwrap_or(0);
        let answer = if buggy && n == 500 { 0 } else { n * 2 };
        MockSandbox::ok(format!("{answer}\n"))
    })));
    let judge =
        Judge::new(sandbox, Arc::new(LanguageRegistry::from_toml(LANGS).unwrap()), JudgeConfig::default());
    let root = problems_dir(tag);
    let problems = ProblemSet::load(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    let config = ServerConfig { max_source_bytes: 1000, run_queue_capacity: 2, ..Default::default() };
    let state = AppState::new(config, Store::open_in_memory().unwrap(), judge, problems);
    worker::spawn_workers(state.clone(), workers);
    http::router(state)
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let (status, body) = call(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn post(app: &Router, uri: &str, body: Value, key: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::post(uri).header("content-type", "application/json");
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    let (status, body) = call(app, req.body(Body::from(body.to_string())).unwrap()).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn wait_done(app: &Router, uri: &str) -> Value {
    for _ in 0..200 {
        let (status, body) = get(app, uri).await;
        assert_eq!(status, StatusCode::OK);
        if body["status"] == "DONE" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{uri} never reached DONE");
}

fn submission() -> Value {
    json!({ "problem": "double", "language": "python", "source": "print(int(input()) * 2)" })
}

#[tokio::test]
async fn submit_is_async_and_reaches_accepted() {
    let app = app("ac", false, 1);
    let (status, created) = post(&app, "/v1/submissions", submission(), None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(created["tests_total"], 3);
    let id = created["id"].as_str().unwrap();

    let done = wait_done(&app, &format!("/v1/submissions/{id}")).await;
    assert_eq!(done["verdict"], "AC");
    assert_eq!(done["tests_done"], 3);
    assert_eq!(done["attempt"], 1);
    assert_eq!(done["tests"].as_array().unwrap().len(), 3);
    assert!(done["judge_version"].as_str().unwrap().starts_with("codexec-judge/"));

    let (_, source) =
        call(&app, Request::get(format!("/v1/submissions/{id}/source")).body(Body::empty()).unwrap()).await;
    assert_eq!(source, "print(int(input()) * 2)");

    let (_, list) = get(&app, "/v1/submissions?problem=double").await;
    assert_eq!(list["submissions"].as_array().unwrap().len(), 1);
    assert!(list["submissions"][0].get("tests").is_none(), "listings omit per-test detail");
}

#[tokio::test]
async fn wrong_answer_stops_early_and_shows_the_failing_input() {
    let app = app("wa", true, 1);
    let (_, created) = post(&app, "/v1/submissions", submission(), None).await;
    let done = wait_done(&app, &format!("/v1/submissions/{}", created["id"].as_str().unwrap())).await;
    assert_eq!(done["verdict"], "WA");
    assert_eq!(done["failed_test"], "03");
    let last = &done["tests"][2];
    assert_eq!(last["input"], "500\n");
    assert_eq!(last["expected"], "1000\n");
    assert_eq!(last["stdout"], "0\n");
    assert!(done["tests"][0].get("input").is_none());
}

#[tokio::test]
async fn sse_replays_the_verdict_and_closes() {
    let app = app("sse", false, 1);
    let (_, created) = post(&app, "/v1/submissions", submission(), None).await;
    let id = created["id"].as_str().unwrap().to_string();
    wait_done(&app, &format!("/v1/submissions/{id}")).await;

    let uri = format!("/v1/submissions/{id}/events");
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(5),
        call(&app, Request::get(uri).body(Body::empty()).unwrap()),
    )
    .await
    .expect("stream must close after the verdict");
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("event: verdict"), "{body}");
    assert!(body.contains("\"verdict\":\"AC\""), "{body}");
}

#[tokio::test]
async fn idempotency_key_deduplicates() {
    let app = app("idem", false, 1);
    let (_, a) = post(&app, "/v1/submissions", submission(), Some("retry-1")).await;
    let (_, b) = post(&app, "/v1/submissions", submission(), Some("retry-1")).await;
    let (_, c) = post(&app, "/v1/submissions", submission(), Some("retry-2")).await;
    assert_eq!(a["id"], b["id"]);
    assert_ne!(a["id"], c["id"]);
}

#[tokio::test]
async fn validation_errors() {
    let app = app("val", false, 1);
    let mut bad = submission();
    bad["language"] = json!("cobol");
    assert_eq!(post(&app, "/v1/submissions", bad, None).await.0, StatusCode::BAD_REQUEST);

    let mut bad = submission();
    bad["problem"] = json!("nope");
    assert_eq!(post(&app, "/v1/submissions", bad, None).await.0, StatusCode::NOT_FOUND);

    let mut bad = submission();
    bad["source"] = json!("   ");
    assert_eq!(post(&app, "/v1/submissions", bad, None).await.0, StatusCode::BAD_REQUEST);

    let mut bad = submission();
    bad["source"] = json!("x".repeat(1001));
    assert_eq!(post(&app, "/v1/submissions", bad, None).await.0, StatusCode::PAYLOAD_TOO_LARGE);

    let mut bad = submission();
    bad["extra"] = json!(1);
    assert!(post(&app, "/v1/submissions", bad, None).await.0.is_client_error());

    assert_eq!(get(&app, "/v1/submissions/does-not-exist").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/v1/runs/does-not-exist").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn run_with_custom_stdin_returns_output() {
    let app = app("run", false, 1);
    let body = json!({ "language": "python", "source": "print(1)", "stdin": "7\n" });
    let (status, created) = post(&app, "/v1/runs", body, None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let done = wait_done(&app, &format!("/v1/runs/{}", created["id"].as_str().unwrap())).await;
    assert_eq!(done["result"]["verdict"], "AC");
    assert_eq!(done["result"]["tests"][0]["stdout"], "14\n");
    assert!(done["result"]["tests"][0].get("expected").is_none());
}

#[tokio::test]
async fn run_against_samples_compares_output() {
    let app = app("samples", false, 1);
    let body = json!({ "language": "python", "source": "print(1)", "problem": "double" });
    let (_, created) = post(&app, "/v1/runs", body, None).await;
    let done = wait_done(&app, &format!("/v1/runs/{}", created["id"].as_str().unwrap())).await;
    let tests = done["result"]["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "only the sample, never hidden tests");
    assert_eq!(tests[0]["name"], "01");
    assert_eq!(tests[0]["expected"], "2\n");
}

#[tokio::test]
async fn run_lane_is_bounded() {
    let app = app("full", false, 0); // no workers: the queue can only fill up
    let body = json!({ "language": "python", "source": "print(1)" });
    assert_eq!(post(&app, "/v1/runs", body.clone(), None).await.0, StatusCode::ACCEPTED);
    assert_eq!(post(&app, "/v1/runs", body.clone(), None).await.0, StatusCode::ACCEPTED);
    assert_eq!(post(&app, "/v1/runs", body, None).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn catalogue_endpoints() {
    let app = app("cat", false, 0);
    let (_, health) = get(&app, "/healthz").await;
    assert_eq!(health["status"], "ok");
    let (_, langs) = get(&app, "/v1/languages").await;
    assert_eq!(langs["languages"][0]["id"], "python");
    let (_, list) = get(&app, "/v1/problems").await;
    assert_eq!(list["problems"][0]["slug"], "double");
    let (_, p) = get(&app, "/v1/problems/double").await;
    assert_eq!(p["samples"].as_array().unwrap().len(), 1);
    assert_eq!(p["tests_total"], 3);
    assert!(p.get("tests").is_none(), "hidden tests are never served");
}

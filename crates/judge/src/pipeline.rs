//! The judging pipeline: compile, run each test, compare, stop at the first failure.
//!
//! `judge()` is a pure function of (source, language config, tests, limits),
//! which is what makes retries, duplicate deliveries and rejudges safe.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::compare::{compare, Comparer};
use crate::language::{Language, LanguageRegistry, Step};
use crate::sandbox::{ExecLimits, ExecOutcome, ExecSpec, ExecStatus, Sandbox, SandboxBox, SandboxError};
use crate::verdict::{JudgeResult, TestResult, Verdict};

#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub input: Vec<u8>,
    /// `None` means "just execute" (Run with custom input): no comparison.
    pub expected: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JudgeConfig {
    pub compile_cpu_time_ms: u64,
    pub compile_wall_time_ms: u64,
    pub compile_memory_kb: u64,
    pub compile_processes: u32,
    /// Bytes of compiler output kept.
    pub compile_output_bytes: usize,
    pub run_processes: u32,
    pub file_size_kb: u64,
    /// stdout above this is an output-limit violation.
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    /// Max bytes of input/expected/stdout/stderr copied into a `TestResult`.
    pub excerpt_bytes: usize,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            compile_cpu_time_ms: 15_000,
            compile_wall_time_ms: 40_000,
            compile_memory_kb: 1024 * 1024,
            compile_processes: 128,
            compile_output_bytes: 16 * 1024,
            run_processes: 64,
            file_size_kb: 64 * 1024,
            stdout_bytes: 16 * 1024 * 1024,
            stderr_bytes: 64 * 1024,
            excerpt_bytes: 1024,
        }
    }
}

#[derive(Debug)]
pub struct JudgeRequest<'a> {
    pub language: &'a str,
    pub source: &'a str,
    pub tests: &'a [TestCase],
    /// Problem limit before the language multiplier.
    pub time_limit_ms: u64,
    pub memory_limit_kb: u64,
    pub comparer: Comparer,
    /// LeetCode semantics: stop at the first non-AC test.
    pub stop_on_first_failure: bool,
    /// Attach stdout/stderr excerpts to every test (Run mode).
    pub capture_output: bool,
    /// Attach input/expected/stdout to the first failing test.
    pub show_failed_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Compiling,
    Running { done: usize, total: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("unknown language {0:?}")]
    UnknownLanguage(String),
    /// Infrastructure failure. The caller should retry, then report IE.
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
}

#[derive(Clone)]
pub struct Judge {
    sandbox: Arc<dyn Sandbox>,
    languages: Arc<LanguageRegistry>,
    cfg: JudgeConfig,
}

impl Judge {
    pub fn new(sandbox: Arc<dyn Sandbox>, languages: Arc<LanguageRegistry>, cfg: JudgeConfig) -> Self {
        Self { sandbox, languages, cfg }
    }

    pub fn languages(&self) -> &LanguageRegistry {
        &self.languages
    }

    pub async fn judge(
        &self,
        req: &JudgeRequest<'_>,
        progress: Option<&UnboundedSender<Progress>>,
    ) -> Result<JudgeResult, JudgeError> {
        let lang = self
            .languages
            .get(req.language)
            .ok_or_else(|| JudgeError::UnknownLanguage(req.language.to_string()))?;

        let mut bx = self.sandbox.acquire().await?;
        let result = self.judge_in_box(bx.as_mut(), lang, req, progress).await;
        if let Err(e) = bx.release().await {
            tracing::warn!(error = %e, "sandbox release failed; the next init cleans the box");
        }
        result
    }

    async fn judge_in_box(
        &self,
        bx: &mut dyn SandboxBox,
        lang: &Language,
        req: &JudgeRequest<'_>,
        progress: Option<&UnboundedSender<Progress>>,
    ) -> Result<JudgeResult, JudgeError> {
        let notify = |p: Progress| {
            if let Some(tx) = progress {
                let _ = tx.send(p);
            }
        };
        let total = req.tests.len();

        bx.write_file(&lang.source_file, req.source.as_bytes()).await?;

        let mut compile_output = None;
        if let Some(step) = &lang.compile {
            notify(Progress::Compiling);
            let out = bx.exec(&self.compile_spec(lang, step), b"").await?;
            let text = excerpt(&out.stdout, self.cfg.compile_output_bytes);
            if out.status != ExecStatus::Ok {
                let reason = match out.status {
                    ExecStatus::TimeLimit | ExecStatus::WallTimeLimit => "compilation timed out\n",
                    ExecStatus::MemoryLimit => "compiler ran out of memory\n",
                    _ => "",
                };
                return Ok(JudgeResult {
                    verdict: Verdict::CompileError,
                    time_ms: 0,
                    memory_kb: 0,
                    failed_test: None,
                    compile_output: Some(format!("{reason}{text}")),
                    tests: Vec::new(),
                    tests_total: total,
                });
            }
            compile_output = Some(text).filter(|t| !t.is_empty());
        }

        let run_spec = self.run_spec(lang, req);
        let mut results = Vec::with_capacity(total);
        let mut failed_test = None;
        notify(Progress::Running { done: 0, total });

        for (i, test) in req.tests.iter().enumerate() {
            let out = bx.exec(&run_spec, &test.input).await?;
            let (verdict, message) = classify(&out, test, req.comparer);
            let first_failure = verdict != Verdict::Accepted && failed_test.is_none();
            if first_failure {
                failed_test = Some(test.name.clone());
            }

            let show_io = req.capture_output || (first_failure && req.show_failed_input);
            let n = self.cfg.excerpt_bytes;
            results.push(TestResult {
                name: test.name.clone(),
                verdict,
                time_ms: out.cpu_time_ms,
                memory_kb: out.memory_kb,
                message,
                input: show_io.then(|| excerpt(&test.input, n)),
                expected: (show_io).then(|| test.expected.as_deref().map(|e| excerpt(e, n))).flatten(),
                stdout: show_io.then(|| excerpt(&out.stdout, n)),
                stderr: (show_io || verdict == Verdict::RuntimeError)
                    .then(|| excerpt(&out.stderr, n))
                    .filter(|s| !s.is_empty()),
            });
            notify(Progress::Running { done: i + 1, total });

            if first_failure && req.stop_on_first_failure {
                break;
            }
        }

        let verdict =
            results.iter().map(|r| r.verdict).find(|v| *v != Verdict::Accepted).unwrap_or(Verdict::Accepted);
        Ok(JudgeResult {
            verdict,
            time_ms: results.iter().map(|r| r.time_ms).max().unwrap_or(0),
            memory_kb: results.iter().map(|r| r.memory_kb).max().unwrap_or(0),
            failed_test,
            compile_output,
            tests: results,
            tests_total: total,
        })
    }

    fn compile_spec(&self, lang: &Language, step: &Step) -> ExecSpec {
        let c = &self.cfg;
        ExecSpec {
            argv: step.argv.clone(),
            env: step.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            dirs: step.dirs.clone(),
            limits: ExecLimits {
                cpu_time: Duration::from_millis(c.compile_cpu_time_ms),
                extra_time: Duration::from_millis(500),
                wall_time: Duration::from_millis(c.compile_wall_time_ms),
                memory_kb: c.compile_memory_kb,
                processes: step.processes.unwrap_or(c.compile_processes),
                file_size_kb: c.file_size_kb.max(256 * 1024),
                stdout_bytes: c.compile_output_bytes,
                stderr_bytes: c.compile_output_bytes,
                address_space_limit: lang.address_space_limit,
            },
            stderr_to_stdout: true,
            syscall_flags: step.syscall_flags,
        }
    }

    fn run_spec(&self, lang: &Language, req: &JudgeRequest<'_>) -> ExecSpec {
        let c = &self.cfg;
        let cpu_ms = (req.time_limit_ms as f64 * lang.time_multiplier).ceil() as u64;
        ExecSpec {
            argv: lang.run.argv.clone(),
            env: lang.run.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            dirs: lang.run.dirs.clone(),
            limits: ExecLimits {
                cpu_time: Duration::from_millis(cpu_ms),
                extra_time: Duration::from_millis(500),
                // Guards against sleeping programs; CPU time is the real limit.
                wall_time: Duration::from_millis(cpu_ms * 2 + 1000),
                memory_kb: req.memory_limit_kb,
                processes: lang.run.processes.unwrap_or(c.run_processes),
                file_size_kb: c.file_size_kb,
                stdout_bytes: c.stdout_bytes,
                stderr_bytes: c.stderr_bytes,
                address_space_limit: lang.address_space_limit,
            },
            stderr_to_stdout: false,
            syscall_flags: lang.run.syscall_flags,
        }
    }
}

fn classify(out: &ExecOutcome, test: &TestCase, comparer: Comparer) -> (Verdict, Option<String>) {
    match &out.status {
        ExecStatus::Ok => match &test.expected {
            None => (Verdict::Accepted, None),
            Some(expected) => match compare(comparer, expected, &out.stdout) {
                Ok(()) => (Verdict::Accepted, None),
                Err(why) => (Verdict::WrongAnswer, Some(why)),
            },
        },
        ExecStatus::NonZeroExit(code) => (Verdict::RuntimeError, Some(format!("exit code {code}"))),
        ExecStatus::Signaled(sig) => (Verdict::RuntimeError, Some(format!("killed by signal {sig}"))),
        ExecStatus::TimeLimit => (Verdict::TimeLimitExceeded, None),
        ExecStatus::WallTimeLimit => {
            (Verdict::TimeLimitExceeded, Some("wall-clock limit (sleeping or blocked)".into()))
        }
        ExecStatus::MemoryLimit => (Verdict::MemoryLimitExceeded, None),
        ExecStatus::OutputLimit => (Verdict::OutputLimitExceeded, None),
    }
}

/// Lossy UTF-8 excerpt. Always rendered as text by clients, never as HTML.
fn excerpt(data: &[u8], max: usize) -> String {
    if data.len() <= max {
        String::from_utf8_lossy(data).into_owned()
    } else {
        format!("{}\n... [truncated, {} bytes total]", String::from_utf8_lossy(&data[..max]), data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockSandbox, Script};

    const LANGS: &str = r#"
[python]
display_name = "Python"
source_file = "main.py"
time_multiplier = 3.0
[python.run]
argv = ["python3", "main.py"]

[rust]
display_name = "Rust"
source_file = "main.rs"
[rust.compile]
argv = ["rustc", "main.rs"]
[rust.run]
argv = ["./main"]
"#;

    fn judge(script: Script) -> (Judge, Arc<MockSandbox>) {
        let sandbox = Arc::new(MockSandbox::new(script));
        let langs = Arc::new(LanguageRegistry::from_toml(LANGS).unwrap());
        (Judge::new(sandbox.clone(), langs, JudgeConfig::default()), sandbox)
    }

    fn tests3() -> Vec<TestCase> {
        (1..=3)
            .map(|i| TestCase {
                name: format!("{i:02}"),
                input: format!("{i}\n").into_bytes(),
                expected: Some(format!("{}\n", i * 2).into_bytes()),
            })
            .collect()
    }

    fn req<'a>(lang: &'a str, tests: &'a [TestCase]) -> JudgeRequest<'a> {
        JudgeRequest {
            language: lang,
            source: "src",
            tests,
            time_limit_ms: 1000,
            memory_limit_kb: 262_144,
            comparer: Comparer::Lines,
            stop_on_first_failure: true,
            capture_output: false,
            show_failed_input: false,
        }
    }

    /// Doubles its input, like a correct solution would.
    fn doubler(_spec: &ExecSpec, stdin: &[u8]) -> ExecOutcome {
        let n: i64 = String::from_utf8_lossy(stdin).trim().parse().unwrap_or(0);
        MockSandbox::ok(format!("{}\n", n * 2))
    }

    #[tokio::test]
    async fn accepted_runs_every_test_and_applies_multiplier() {
        let (judge, sandbox) = judge(Box::new(doubler));
        let tests = tests3();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = judge.judge(&req("python", &tests), Some(&tx)).await.unwrap();
        assert_eq!(res.verdict, Verdict::Accepted);
        assert_eq!(res.tests.len(), 3);
        assert!(res.failed_test.is_none());

        let calls = sandbox.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].limits.cpu_time, Duration::from_millis(3000), "1000 ms x 3.0");
        assert_eq!(calls[0].limits.wall_time, Duration::from_millis(7000));
        assert_eq!(sandbox.files(), vec![("main.py".to_string(), b"src".to_vec())]);
        assert_eq!(sandbox.released(), 1);

        drop(tx);
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert_eq!(events.first(), Some(&Progress::Running { done: 0, total: 3 }));
        assert_eq!(events.last(), Some(&Progress::Running { done: 3, total: 3 }));
    }

    #[tokio::test]
    async fn stops_at_first_failure() {
        let (judge, sandbox) =
            judge(Box::new(
                |spec, stdin| {
                    if stdin == b"2\n" {
                        MockSandbox::ok("999\n")
                    } else {
                        doubler(spec, stdin)
                    }
                },
            ));
        let tests = tests3();
        let mut r = req("python", &tests);
        r.show_failed_input = true;
        let res = judge.judge(&r, None).await.unwrap();
        assert_eq!(res.verdict, Verdict::WrongAnswer);
        assert_eq!(res.failed_test.as_deref(), Some("02"));
        assert_eq!(res.tests.len(), 2, "third test must not run");
        assert_eq!(sandbox.calls().len(), 2);
        assert_eq!(res.tests[1].input.as_deref(), Some("2\n"));
        assert_eq!(res.tests[1].expected.as_deref(), Some("4\n"));
        assert!(res.tests[0].input.is_none(), "passing tests never expose data");
    }

    #[tokio::test]
    async fn hidden_tests_stay_hidden_by_default() {
        let (judge, _) = judge(Box::new(|_, _| MockSandbox::ok("nope\n")));
        let tests = tests3();
        let res = judge.judge(&req("python", &tests), None).await.unwrap();
        assert_eq!(res.verdict, Verdict::WrongAnswer);
        let t = &res.tests[0];
        assert!(t.input.is_none() && t.expected.is_none() && t.stdout.is_none());
    }

    #[tokio::test]
    async fn runs_all_tests_when_asked() {
        let (judge, _) = judge(Box::new(|_, _| MockSandbox::ok("nope\n")));
        let tests = tests3();
        let mut r = req("python", &tests);
        r.stop_on_first_failure = false;
        let res = judge.judge(&r, None).await.unwrap();
        assert_eq!(res.tests.len(), 3);
        assert_eq!(res.failed_test.as_deref(), Some("01"));
    }

    #[tokio::test]
    async fn compile_error_short_circuits() {
        let (judge, sandbox) = judge(Box::new(|spec, _| {
            if spec.argv[0] == "rustc" {
                MockSandbox::with_status(ExecStatus::NonZeroExit(1), "error[E0425]: nope")
            } else {
                panic!("must not run after a failed compile")
            }
        }));
        let tests = tests3();
        let res = judge.judge(&req("rust", &tests), None).await.unwrap();
        assert_eq!(res.verdict, Verdict::CompileError);
        assert!(res.compile_output.unwrap().contains("E0425"));
        assert!(res.tests.is_empty());
        assert!(sandbox.calls()[0].stderr_to_stdout);
        assert_eq!(sandbox.released(), 1);
    }

    #[tokio::test]
    async fn status_mapping() {
        for (status, want) in [
            (ExecStatus::TimeLimit, Verdict::TimeLimitExceeded),
            (ExecStatus::WallTimeLimit, Verdict::TimeLimitExceeded),
            (ExecStatus::MemoryLimit, Verdict::MemoryLimitExceeded),
            (ExecStatus::OutputLimit, Verdict::OutputLimitExceeded),
            (ExecStatus::Signaled(11), Verdict::RuntimeError),
            (ExecStatus::NonZeroExit(2), Verdict::RuntimeError),
        ] {
            let s = status.clone();
            let (judge, _) = judge(Box::new(move |_, _| MockSandbox::with_status(s.clone(), "")));
            let tests = tests3();
            let res = judge.judge(&req("python", &tests), None).await.unwrap();
            assert_eq!(res.verdict, want, "{status:?}");
        }
    }

    #[tokio::test]
    async fn run_mode_captures_output_without_comparison() {
        let (judge, _) =
            judge(Box::new(|_, stdin| MockSandbox::ok(format!("echo:{}", String::from_utf8_lossy(stdin)))));
        let tests = vec![TestCase { name: "custom".into(), input: b"hi".to_vec(), expected: None }];
        let mut r = req("python", &tests);
        r.capture_output = true;
        let res = judge.judge(&r, None).await.unwrap();
        assert_eq!(res.verdict, Verdict::Accepted);
        assert_eq!(res.tests[0].stdout.as_deref(), Some("echo:hi"));
        assert!(res.tests[0].expected.is_none());
    }

    #[tokio::test]
    async fn sandbox_failure_is_an_error_not_a_verdict() {
        let sandbox = Arc::new(MockSandbox::failing("isolate exploded"));
        let langs = Arc::new(LanguageRegistry::from_toml(LANGS).unwrap());
        let judge = Judge::new(sandbox.clone(), langs, JudgeConfig::default());
        let tests = tests3();
        let err = judge.judge(&req("python", &tests), None).await.unwrap_err();
        assert!(matches!(err, JudgeError::Sandbox(_)));
        assert_eq!(sandbox.released(), 1, "box is released even on failure");

        let err = judge.judge(&req("cobol", &tests), None).await.unwrap_err();
        assert!(matches!(err, JudgeError::UnknownLanguage(_)));
    }

    #[test]
    fn excerpt_truncates() {
        assert_eq!(excerpt(b"abc", 10), "abc");
        let long = excerpt(&[b'x'; 5000], 10);
        assert!(long.starts_with("xxxxxxxxxx\n") && long.contains("5000 bytes"));
    }
}

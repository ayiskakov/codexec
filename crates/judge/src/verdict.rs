use serde::{Deserialize, Serialize};

/// Final outcome of a submission or of one test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Verdict {
    /// Accepted.
    #[serde(rename = "AC")]
    Accepted,
    /// Wrong answer.
    #[serde(rename = "WA")]
    WrongAnswer,
    /// CPU or wall-clock time limit exceeded.
    #[serde(rename = "TLE")]
    TimeLimitExceeded,
    /// Memory limit exceeded.
    #[serde(rename = "MLE")]
    MemoryLimitExceeded,
    /// Runtime error: non-zero exit code or fatal signal.
    #[serde(rename = "RE")]
    RuntimeError,
    /// Compilation error.
    #[serde(rename = "CE")]
    CompileError,
    /// Output limit exceeded.
    #[serde(rename = "OLE")]
    OutputLimitExceeded,
    /// Internal error: the judge failed, not the submission.
    #[serde(rename = "IE")]
    InternalError,
}

impl Verdict {
    pub fn code(self) -> &'static str {
        match self {
            Verdict::Accepted => "AC",
            Verdict::WrongAnswer => "WA",
            Verdict::TimeLimitExceeded => "TLE",
            Verdict::MemoryLimitExceeded => "MLE",
            Verdict::RuntimeError => "RE",
            Verdict::CompileError => "CE",
            Verdict::OutputLimitExceeded => "OLE",
            Verdict::InternalError => "IE",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "AC" => Verdict::Accepted,
            "WA" => Verdict::WrongAnswer,
            "TLE" => Verdict::TimeLimitExceeded,
            "MLE" => Verdict::MemoryLimitExceeded,
            "RE" => Verdict::RuntimeError,
            "CE" => Verdict::CompileError,
            "OLE" => Verdict::OutputLimitExceeded,
            "IE" => Verdict::InternalError,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub verdict: Verdict,
    pub time_ms: u64,
    pub memory_kb: u64,
    /// Short human-readable reason, e.g. "exit code 1" or "line 3 differs".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Captured only when the request asks for it (Run mode, or the first
    /// failing test of a problem that allows showing it). Always truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeResult {
    pub verdict: Verdict,
    /// Maximum CPU time over executed tests.
    pub time_ms: u64,
    /// Maximum resident memory over executed tests.
    pub memory_kb: u64,
    /// Name of the first test that did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_test: Option<String>,
    /// Compiler output, present for compiled languages (truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_output: Option<String>,
    pub tests: Vec<TestResult>,
    pub tests_total: usize,
}

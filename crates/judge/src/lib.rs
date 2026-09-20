//! codexec-judge: the execution engine.
//!
//! The crate is split along the trust boundary described in the design doc:
//!
//! * [`sandbox`] defines what "run this untrusted command" means, independent
//!   of the isolation technology.
//! * [`isolate`] is the production backend. It drives the `isolate` binary
//!   (namespaces + cgroup v2 + seccomp) and never reads a path that lives
//!   inside the sandbox: input goes in through a pipe, output comes back
//!   through pipes, and resource usage comes from a meta file that only the
//!   host can write.
//! * [`pipeline`] is the pure function `verdict = f(source, language, tests)`.
//!   Expected outputs never enter the sandbox; comparison happens here.

pub mod compare;
pub mod isolate;
pub mod language;
pub mod pipeline;
pub mod problem;
pub mod sandbox;
pub mod verdict;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use compare::Comparer;
pub use isolate::{IsolateConfig, IsolateSandbox};
pub use language::{Language, LanguageRegistry};
pub use pipeline::{Judge, JudgeConfig, JudgeError, JudgeRequest, Progress, TestCase};
pub use problem::{Problem, ProblemSet};
pub use sandbox::{ExecLimits, ExecOutcome, ExecSpec, ExecStatus, Sandbox, SandboxBox, SandboxError};
pub use verdict::{JudgeResult, TestResult, Verdict};

/// Recorded on every submission so a verdict can be traced to the engine that produced it.
pub const JUDGE_VERSION: &str = concat!("codexec-judge/", env!("CARGO_PKG_VERSION"));

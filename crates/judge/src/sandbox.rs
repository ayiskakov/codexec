//! Technology-independent description of "run this untrusted command".

use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid file name for sandbox: {0:?}")]
    InvalidFileName(String),
    #[error("sandbox backend failed: {0}")]
    Backend(String),
}

/// Resource limits for one execution.
#[derive(Debug, Clone)]
pub struct ExecLimits {
    pub cpu_time: Duration,
    /// Grace period after `cpu_time` so slightly-over runs report their real time.
    pub extra_time: Duration,
    pub wall_time: Duration,
    pub memory_kb: u64,
    /// Max processes + threads.
    pub processes: u32,
    /// Max size of any file the program creates.
    pub file_size_kb: u64,
    /// Bytes of stdout kept; more than this is an output-limit violation.
    pub stdout_bytes: usize,
    /// Bytes of stderr kept; the rest is dropped silently.
    pub stderr_bytes: usize,
    /// Only consulted when the backend has no cgroup memory control: whether
    /// the memory limit may be enforced as an address-space limit. Runtimes
    /// that reserve huge virtual ranges (Go, JVM) must set this to false.
    pub address_space_limit: bool,
}

/// One command to execute inside a box.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Extra bind mounts in backend syntax (isolate: `inside=outside[:opts]`).
    pub dirs: Vec<String>,
    pub limits: ExecLimits,
    /// Merge stderr into stdout (used for compiler output).
    pub stderr_to_stdout: bool,
    /// Backend-specific syscall restriction override (isolate `--syscalls`).
    pub syscall_flags: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecStatus {
    Ok,
    NonZeroExit(i32),
    Signaled(i32),
    TimeLimit,
    WallTimeLimit,
    MemoryLimit,
    OutputLimit,
}

#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub status: ExecStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cpu_time_ms: u64,
    pub wall_time_ms: u64,
    pub memory_kb: u64,
}

/// A pool of isolated boxes. One box is used for one submission at a time.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Waits for a free slot and returns a freshly initialised, empty box.
    async fn acquire(&self) -> Result<Box<dyn SandboxBox>, SandboxError>;
}

#[async_trait]
pub trait SandboxBox: Send {
    /// Writes a file into the box working directory.
    ///
    /// Must only be called before untrusted code has run in this box: after
    /// that point the directory is hostile (symlinks, renamed files) and the
    /// host must not open paths inside it.
    async fn write_file(&mut self, name: &str, contents: &[u8]) -> Result<(), SandboxError>;

    /// Runs one command. `stdin` is fed through a pipe; stdout and stderr
    /// come back through pipes, capped per `spec.limits`.
    async fn exec(&mut self, spec: &ExecSpec, stdin: &[u8]) -> Result<ExecOutcome, SandboxError>;

    /// Destroys the box contents and returns the slot to the pool.
    async fn release(self: Box<Self>) -> Result<(), SandboxError>;
}

/// Accepts plain file names only: no separators, no traversal, no hidden files.
pub fn validate_file_name(name: &str) -> Result<(), SandboxError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(SandboxError::InvalidFileName(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert!(validate_file_name("main.py").is_ok());
        assert!(validate_file_name("Main_1-x.rs").is_ok());
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a\\b", "a b", "a\0b"] {
            assert!(validate_file_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}

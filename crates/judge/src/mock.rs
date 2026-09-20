//! Scripted sandbox for tests. Never compiled into release binaries unless
//! the `mock` feature is requested.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::sandbox::{
    validate_file_name, ExecOutcome, ExecSpec, ExecStatus, Sandbox, SandboxBox, SandboxError,
};

/// Decides the outcome of every exec from its spec and stdin.
pub type Script = Box<dyn Fn(&ExecSpec, &[u8]) -> ExecOutcome + Send + Sync>;

#[derive(Default)]
struct State {
    calls: Vec<ExecSpec>,
    files: Vec<(String, Vec<u8>)>,
    acquired: usize,
    released: usize,
}

pub struct MockSandbox {
    script: Option<Arc<Script>>,
    fail_with: Option<String>,
    state: Arc<Mutex<State>>,
}

impl MockSandbox {
    pub fn new(script: Script) -> Self {
        Self { script: Some(Arc::new(script)), fail_with: None, state: Default::default() }
    }

    /// Every exec fails with an infrastructure error.
    pub fn failing(message: &str) -> Self {
        Self { script: None, fail_with: Some(message.to_string()), state: Default::default() }
    }

    pub fn ok(stdout: impl Into<Vec<u8>>) -> ExecOutcome {
        ExecOutcome {
            status: ExecStatus::Ok,
            stdout: stdout.into(),
            stderr: Vec::new(),
            cpu_time_ms: 10,
            wall_time_ms: 12,
            memory_kb: 4096,
        }
    }

    pub fn with_status(status: ExecStatus, stdout: impl Into<Vec<u8>>) -> ExecOutcome {
        ExecOutcome { status, ..Self::ok(stdout) }
    }

    pub fn calls(&self) -> Vec<ExecSpec> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        self.state.lock().unwrap().files.clone()
    }

    pub fn acquired(&self) -> usize {
        self.state.lock().unwrap().acquired
    }

    pub fn released(&self) -> usize {
        self.state.lock().unwrap().released
    }
}

#[async_trait]
impl Sandbox for MockSandbox {
    async fn acquire(&self) -> Result<Box<dyn SandboxBox>, SandboxError> {
        self.state.lock().unwrap().acquired += 1;
        Ok(Box::new(MockBox {
            script: self.script.clone(),
            fail_with: self.fail_with.clone(),
            state: self.state.clone(),
        }))
    }
}

struct MockBox {
    script: Option<Arc<Script>>,
    fail_with: Option<String>,
    state: Arc<Mutex<State>>,
}

#[async_trait]
impl SandboxBox for MockBox {
    async fn write_file(&mut self, name: &str, contents: &[u8]) -> Result<(), SandboxError> {
        validate_file_name(name)?;
        self.state.lock().unwrap().files.push((name.to_string(), contents.to_vec()));
        Ok(())
    }

    async fn exec(&mut self, spec: &ExecSpec, stdin: &[u8]) -> Result<ExecOutcome, SandboxError> {
        self.state.lock().unwrap().calls.push(spec.clone());
        if let Some(msg) = &self.fail_with {
            return Err(SandboxError::Backend(msg.clone()));
        }
        let script = self.script.as_ref().expect("script or failure configured");
        Ok(script(spec, stdin))
    }

    async fn release(self: Box<Self>) -> Result<(), SandboxError> {
        self.state.lock().unwrap().released += 1;
        Ok(())
    }
}

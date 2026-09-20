//! Production sandbox backend: drives the `isolate` binary (>= 2.0, tested with 2.7).
//!
//! Hard rules, taken from the Judge0 escape post-mortem:
//!
//! * The worker never opens a path inside the box after untrusted code ran.
//!   stdin/stdout/stderr travel through pipes, and the meta file lives in a
//!   host-only directory.
//! * Files are written into the box only while it is still pristine, with
//!   `O_EXCL`, and only under validated plain file names.
//! * No privileged containers. `isolate` is setuid root; this process is not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::sandbox::{
    validate_file_name, ExecLimits, ExecOutcome, ExecSpec, ExecStatus, Sandbox, SandboxBox, SandboxError,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolateConfig {
    /// Path to the isolate binary.
    #[serde(default = "default_isolate_path")]
    pub isolate_path: PathBuf,
    /// Use cgroup v2 memory control (`--cg`). Required for correct memory
    /// limits with Go and other runtimes that reserve large address ranges.
    /// `false` falls back to rlimits and is meant for development only.
    #[serde(default = "default_true")]
    pub use_cgroups: bool,
    /// Box ids `first_box_id .. first_box_id + slots` belong to this process.
    #[serde(default)]
    pub first_box_id: u32,
    /// Number of concurrent boxes. Rule of thumb: physical cores minus one.
    #[serde(default = "default_slots")]
    pub slots: u32,
    /// Host-only directory for isolate meta files.
    #[serde(default = "default_meta_dir")]
    pub meta_dir: PathBuf,
    /// Bind mounts added to every execution, isolate `--dir` syntax.
    #[serde(default)]
    pub extra_dirs: Vec<String>,
}

fn default_isolate_path() -> PathBuf {
    PathBuf::from("isolate")
}
fn default_true() -> bool {
    true
}
fn default_slots() -> u32 {
    2
}
fn default_meta_dir() -> PathBuf {
    std::env::temp_dir().join("codexec-meta")
}

impl Default for IsolateConfig {
    fn default() -> Self {
        Self {
            isolate_path: default_isolate_path(),
            use_cgroups: true,
            first_box_id: 0,
            slots: default_slots(),
            meta_dir: default_meta_dir(),
            extra_dirs: Vec::new(),
        }
    }
}

struct Pool {
    free_ids: Mutex<Vec<u32>>,
    permits: Arc<Semaphore>,
}

pub struct IsolateSandbox {
    cfg: Arc<IsolateConfig>,
    pool: Arc<Pool>,
}

impl IsolateSandbox {
    pub fn new(cfg: IsolateConfig) -> Result<Self, SandboxError> {
        if cfg.slots == 0 {
            return Err(SandboxError::Backend("isolate: slots must be at least 1".into()));
        }
        std::fs::create_dir_all(&cfg.meta_dir)?;
        let ids: Vec<u32> = (cfg.first_box_id..cfg.first_box_id + cfg.slots).rev().collect();
        Ok(Self {
            pool: Arc::new(Pool {
                free_ids: Mutex::new(ids),
                permits: Arc::new(Semaphore::new(cfg.slots as usize)),
            }),
            cfg: Arc::new(cfg),
        })
    }

    pub fn slots(&self) -> u32 {
        self.cfg.slots
    }

    /// Runs `isolate --version` to fail fast on a missing or broken install.
    pub async fn check(&self) -> Result<String, SandboxError> {
        let out = Command::new(&self.cfg.isolate_path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| {
            SandboxError::Backend(format!(
                "cannot execute {:?}: {e}. Run scripts/setup-isolate.sh first.",
                self.cfg.isolate_path
            ))
        })?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text.lines().next().unwrap_or("isolate (unknown version)").to_string())
    }
}

#[async_trait]
impl Sandbox for IsolateSandbox {
    async fn acquire(&self) -> Result<Box<dyn SandboxBox>, SandboxError> {
        let permit = self
            .pool
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SandboxError::Backend("sandbox pool closed".into()))?;
        let id = self.pool.free_ids.lock().unwrap().pop().expect("semaphore permit implies a free box id");
        let mut bx = IsolateBox {
            id,
            cfg: self.cfg.clone(),
            pool: self.pool.clone(),
            box_dir: PathBuf::new(),
            executed: false,
            _permit: permit,
        };
        // If init fails, `bx` drops here and the id goes back to the pool.
        bx.init().await?;
        Ok(Box::new(bx))
    }
}

struct IsolateBox {
    id: u32,
    cfg: Arc<IsolateConfig>,
    pool: Arc<Pool>,
    box_dir: PathBuf,
    executed: bool,
    _permit: OwnedSemaphorePermit,
}

impl Drop for IsolateBox {
    fn drop(&mut self) {
        // Runs before `_permit` is released, so the id is back before anyone can pop it.
        // A box dropped without `release()` stays dirty; `init()` always cleans first.
        self.pool.free_ids.lock().unwrap().push(self.id);
    }
}

impl IsolateBox {
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.cfg.isolate_path);
        if self.cfg.use_cgroups {
            cmd.arg("--cg");
        }
        cmd.arg(format!("--box-id={}", self.id));
        cmd.stdin(Stdio::null());
        cmd.kill_on_drop(true);
        cmd
    }

    async fn cleanup(&self) -> Result<(), SandboxError> {
        let out = self.base_command().arg("--cleanup").output().await?;
        if !out.status.success() {
            return Err(SandboxError::Backend(format!(
                "isolate --cleanup failed for box {}: {}",
                self.id,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn init(&mut self) -> Result<(), SandboxError> {
        self.cleanup().await?;
        let out = self.base_command().arg("--init").output().await?;
        if !out.status.success() {
            return Err(SandboxError::Backend(format!(
                "isolate --init failed for box {}: {}",
                self.id,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() || !Path::new(&root).is_absolute() {
            return Err(SandboxError::Backend(format!(
                "isolate --init printed an unexpected box path: {root:?}"
            )));
        }
        self.box_dir = Path::new(&root).join("box");
        self.executed = false;
        Ok(())
    }

    fn meta_path(&self) -> PathBuf {
        self.cfg.meta_dir.join(format!("box-{}.meta", self.id))
    }
}

#[async_trait]
impl SandboxBox for IsolateBox {
    async fn write_file(&mut self, name: &str, contents: &[u8]) -> Result<(), SandboxError> {
        validate_file_name(name)?;
        if self.executed {
            return Err(SandboxError::Backend(
                "refusing to write into a box after code has run in it".into(),
            ));
        }
        let path = self.box_dir.join(name);
        // create_new => O_CREAT|O_EXCL, which never follows a symlink at the final component.
        let mut file =
            tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path).await?;
        file.write_all(contents).await?;
        file.flush().await?;
        Ok(())
    }

    async fn exec(&mut self, spec: &ExecSpec, stdin: &[u8]) -> Result<ExecOutcome, SandboxError> {
        if spec.argv.is_empty() {
            return Err(SandboxError::Backend("empty argv".into()));
        }
        self.executed = true;

        let meta_path = self.meta_path();
        let _ = tokio::fs::remove_file(&meta_path).await;

        let mut cmd = self.base_command();
        cmd.args(build_run_args(&self.cfg, &meta_path, spec));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin_pipe = child.stdin.take().expect("piped stdin");
        let stdout_pipe = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");

        let feed = async move {
            // EPIPE is normal: the program may exit without reading its input.
            let _ = stdin_pipe.write_all(stdin).await;
            let _ = stdin_pipe.shutdown().await;
        };

        // isolate enforces the wall clock itself; this only guards against isolate hanging.
        let guard = spec.limits.wall_time + spec.limits.extra_time + Duration::from_secs(15);
        let joined = tokio::time::timeout(guard, async {
            tokio::join!(
                feed,
                read_capped(stdout_pipe, spec.limits.stdout_bytes),
                read_capped(stderr_pipe, spec.limits.stderr_bytes),
                child.wait(),
            )
        })
        .await;

        let (_, stdout_res, stderr_res, exit) = match joined {
            Ok(parts) => parts,
            Err(_) => {
                return Err(SandboxError::Backend(format!(
                    "isolate did not finish within {guard:?} for box {}",
                    self.id
                )))
            }
        };
        let (stdout, stdout_truncated) = stdout_res?;
        let (stderr, _) = stderr_res?;
        let exit_code = exit?.code();

        let meta_text = tokio::fs::read_to_string(&meta_path).await.unwrap_or_default();
        let meta = parse_meta(&meta_text);

        let (status, cpu_time_ms, wall_time_ms, memory_kb) =
            interpret(&meta, exit_code, &spec.limits, self.cfg.use_cgroups, stdout_truncated).map_err(
                |msg| {
                    SandboxError::Backend(format!(
                        "{msg}; isolate stderr: {}",
                        String::from_utf8_lossy(&stderr).trim()
                    ))
                },
            )?;

        Ok(ExecOutcome { status, stdout, stderr, cpu_time_ms, wall_time_ms, memory_kb })
    }

    async fn release(self: Box<Self>) -> Result<(), SandboxError> {
        self.cleanup().await
        // Drop returns the id and the permit.
    }
}

fn secs(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// Arguments after `isolate [--cg] --box-id=N`. Pure, so it can be unit-tested.
pub(crate) fn build_run_args(cfg: &IsolateConfig, meta_path: &Path, spec: &ExecSpec) -> Vec<String> {
    let l = &spec.limits;
    let mut a = vec![
        "--silent".to_string(),
        format!("--meta={}", meta_path.display()),
        format!("--time={}", secs(l.cpu_time)),
        format!("--extra-time={}", secs(l.extra_time)),
        format!("--wall-time={}", secs(l.wall_time)),
        format!("--processes={}", l.processes.max(1)),
        format!("--fsize={}", l.file_size_kb),
    ];
    if cfg.use_cgroups {
        a.push(format!("--cg-mem={}", l.memory_kb));
    } else if l.address_space_limit {
        a.push(format!("--mem={}", l.memory_kb));
    }
    if let Some(flags) = spec.syscall_flags {
        a.push(format!("--syscalls={flags}"));
    }
    if spec.stderr_to_stdout {
        a.push("--stderr-to-stdout".into());
    }
    for dir in cfg.extra_dirs.iter().chain(spec.dirs.iter()) {
        a.push(format!("--dir={dir}"));
    }
    for (k, v) in &spec.env {
        a.push(format!("--env={k}={v}"));
    }
    a.push("--run".into());
    a.push("--".into());
    a.extend(spec.argv.iter().cloned());
    a
}

pub(crate) fn parse_meta(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Turns isolate's exit code and meta file into a status plus (cpu ms, wall ms, memory kB).
///
/// isolate exits 0 when the program ran and succeeded, 1 when the program
/// failed (RE/SG/TO), and anything else when the sandbox itself broke.
pub(crate) fn interpret(
    meta: &HashMap<String, String>,
    exit_code: Option<i32>,
    limits: &ExecLimits,
    use_cgroups: bool,
    stdout_truncated: bool,
) -> Result<(ExecStatus, u64, u64, u64), String> {
    let status_key = meta.get("status").map(String::as_str);
    match exit_code {
        Some(0) | Some(1) if status_key != Some("XX") => {}
        other => {
            return Err(format!(
                "isolate internal error (exit {:?}, status {:?}, message {:?})",
                other,
                status_key,
                meta.get("message")
            ))
        }
    }
    if meta.is_empty() {
        return Err("isolate produced no meta file".into());
    }

    let ms = |key: &str| -> u64 {
        meta.get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .map(|s| (s * 1000.0).round().max(0.0) as u64)
            .unwrap_or(0)
    };
    let kb = |key: &str| meta.get(key).and_then(|v| v.parse::<u64>().ok());

    let cpu_ms = ms("time");
    let wall_ms = ms("time-wall");
    // max-rss is the per-process peak. cg-mem also counts page cache left over
    // from earlier runs in the same box (the compiler), so it is only a fallback.
    let memory_kb = kb("max-rss").or_else(|| kb("cg-mem")).unwrap_or(0);

    let failed = status_key.is_some();
    let oom = meta.contains_key("cg-oom-killed")
        || memory_kb > limits.memory_kb
        // rlimit mode: allocation failures surface as crashes just under the limit.
        || (!use_cgroups && failed && memory_kb.saturating_mul(10) >= limits.memory_kb.saturating_mul(9));

    let status = match status_key {
        Some("TO") => {
            let wall = meta.get("message").map(|m| m.contains("wall")).unwrap_or(false);
            if wall {
                ExecStatus::WallTimeLimit
            } else {
                ExecStatus::TimeLimit
            }
        }
        _ if oom => ExecStatus::MemoryLimit,
        Some("SG") => ExecStatus::Signaled(meta.get("exitsig").and_then(|v| v.parse().ok()).unwrap_or(0)),
        Some("RE") => ExecStatus::NonZeroExit(meta.get("exitcode").and_then(|v| v.parse().ok()).unwrap_or(1)),
        Some(other) => return Err(format!("unknown isolate status {other:?}")),
        None if stdout_truncated => ExecStatus::OutputLimit,
        None => ExecStatus::Ok,
    };
    Ok((status, cpu_ms, wall_ms, memory_kb))
}

/// Reads to EOF, keeps the first `cap` bytes and discards the rest, so a
/// chatty program can never block on a full pipe or exhaust worker memory.
async fn read_capped<R: AsyncRead + Unpin>(mut r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::with_capacity(cap.min(64 * 1024));
    let mut truncated = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = cap.saturating_sub(kept.len());
        if n > room {
            truncated = true;
        }
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    Ok((kept, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ExecLimits {
        ExecLimits {
            cpu_time: Duration::from_millis(1500),
            extra_time: Duration::from_millis(500),
            wall_time: Duration::from_secs(4),
            memory_kb: 262_144,
            processes: 64,
            file_size_kb: 65_536,
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            address_space_limit: true,
        }
    }

    fn spec() -> ExecSpec {
        ExecSpec {
            argv: vec!["./main".into(), "arg".into()],
            env: vec![("GOMAXPROCS".into(), "1".into())],
            dirs: vec!["/gocache=/var/cache/go".into()],
            limits: limits(),
            stderr_to_stdout: false,
            syscall_flags: Some(65531),
        }
    }

    #[test]
    fn args_cgroup_mode() {
        let cfg = IsolateConfig { extra_dirs: vec!["/etc/alternatives".into()], ..Default::default() };
        let a = build_run_args(&cfg, Path::new("/m/box-0.meta"), &spec());
        assert!(a.contains(&"--cg-mem=262144".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--mem=")));
        assert!(a.contains(&"--time=1.500".to_string()));
        assert!(a.contains(&"--wall-time=4.000".to_string()));
        assert!(a.contains(&"--syscalls=65531".to_string()));
        assert!(a.contains(&"--dir=/etc/alternatives".to_string()));
        assert!(a.contains(&"--dir=/gocache=/var/cache/go".to_string()));
        assert!(a.contains(&"--env=GOMAXPROCS=1".to_string()));
        let sep = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[sep - 1], "--run");
        assert_eq!(&a[sep + 1..], ["./main", "arg"]);
    }

    #[test]
    fn args_rlimit_mode_respects_address_space_flag() {
        let cfg = IsolateConfig { use_cgroups: false, ..Default::default() };
        let mut s = spec();
        let a = build_run_args(&cfg, Path::new("/m"), &s);
        assert!(a.contains(&"--mem=262144".to_string()));
        s.limits.address_space_limit = false;
        let a = build_run_args(&cfg, Path::new("/m"), &s);
        assert!(!a.iter().any(|x| x.starts_with("--mem=") || x.starts_with("--cg-mem=")));
    }

    #[test]
    fn interpret_ok() {
        let m = parse_meta("time:0.013\ntime-wall:0.016\nmax-rss:8344\nexitcode:0\n");
        let r = interpret(&m, Some(0), &limits(), true, false).unwrap();
        assert_eq!(r, (ExecStatus::Ok, 13, 16, 8344));
    }

    #[test]
    fn interpret_failures() {
        let l = limits();
        let tle = parse_meta(
            "status:TO\nmessage:Time limit exceeded\nkilled:1\ntime:0.600\ntime-wall:0.601\nmax-rss:7948\n",
        );
        assert_eq!(interpret(&tle, Some(1), &l, true, false).unwrap().0, ExecStatus::TimeLimit);

        let wall =
            parse_meta("status:TO\nmessage:Time limit exceeded (wall clock)\ntime:0.011\nmax-rss:10\n");
        assert_eq!(interpret(&wall, Some(1), &l, true, false).unwrap().0, ExecStatus::WallTimeLimit);

        let sig = parse_meta("time:0.015\nmax-rss:9028\nexitsig:11\nstatus:SG\n");
        assert_eq!(interpret(&sig, Some(1), &l, true, false).unwrap().0, ExecStatus::Signaled(11));

        let re = parse_meta("time:0.015\nmax-rss:9028\nexitcode:3\nstatus:RE\n");
        assert_eq!(interpret(&re, Some(1), &l, true, false).unwrap().0, ExecStatus::NonZeroExit(3));

        let oom = parse_meta("time:0.2\nmax-rss:200000\ncg-oom-killed:1\nexitsig:9\nstatus:SG\n");
        assert_eq!(interpret(&oom, Some(1), &l, true, false).unwrap().0, ExecStatus::MemoryLimit);

        // rlimit mode: MemoryError crash right under the limit is reported as MLE.
        let near = parse_meta("time:0.161\nmax-rss:252604\nexitcode:1\nstatus:RE\n");
        assert_eq!(interpret(&near, Some(1), &l, false, false).unwrap().0, ExecStatus::MemoryLimit);
        assert_eq!(interpret(&near, Some(1), &l, true, false).unwrap().0, ExecStatus::NonZeroExit(1));

        let ole = parse_meta("time:0.1\nmax-rss:100\nexitcode:0\n");
        assert_eq!(interpret(&ole, Some(0), &l, true, true).unwrap().0, ExecStatus::OutputLimit);
    }

    #[test]
    fn interpret_sandbox_errors() {
        let l = limits();
        let xx = parse_meta("status:XX\nmessage:Cannot run proxy\n");
        assert!(interpret(&xx, Some(2), &l, true, false).is_err());
        assert!(interpret(&xx, Some(1), &l, true, false).is_err());
        assert!(interpret(&HashMap::new(), Some(0), &l, true, false).is_err());
        assert!(interpret(&parse_meta("time:0.1\n"), None, &l, true, false).is_err());
    }

    #[tokio::test]
    async fn capped_reader_drains_everything() {
        let data = vec![b'x'; 200_000];
        let (kept, truncated) = read_capped(&data[..], 1000).await.unwrap();
        assert_eq!(kept.len(), 1000);
        assert!(truncated);
        let (kept, truncated) = read_capped(&data[..10], 1000).await.unwrap();
        assert_eq!(kept.len(), 10);
        assert!(!truncated);
    }
}

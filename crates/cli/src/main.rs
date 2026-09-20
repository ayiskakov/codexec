//! `codexec judge`         judge one source file against a problem directory
//! `codexec check-problem` run every file in `<problem>/solutions/` and verify
//!                         it gets the verdict its file name promises
//!                         (`ac.py`, `wa-off-by-one.go`, `tle.rs`, ...).
//!
//! `check-problem` is the seed of the problem build pipeline from the design
//! doc: tests are only trusted if reference solutions pass and known-bad
//! solutions fail the way they are labelled.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use codexec_judge::{
    IsolateConfig, IsolateSandbox, Judge, JudgeConfig, JudgeRequest, JudgeResult, LanguageRegistry, Problem,
    Verdict,
};
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "codexec", version, about = "Judge code inside the isolate sandbox")]
struct Cli {
    /// Engine configuration; only the [sandbox] and [judge] tables are read.
    #[arg(long, global = true, default_value = "codexec.toml")]
    config: PathBuf,
    /// Language registry.
    #[arg(long, global = true, default_value = "languages.toml")]
    languages: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Judge one source file against all tests of a problem.
    Judge {
        #[arg(long)]
        problem: PathBuf,
        /// Language id; inferred from the file extension when omitted.
        #[arg(long)]
        lang: Option<String>,
        /// Run every test instead of stopping at the first failure.
        #[arg(long)]
        all: bool,
        /// Print the full result as JSON.
        #[arg(long)]
        json: bool,
        source: PathBuf,
    },
    /// Verify that solutions/ get the verdicts their file names promise.
    CheckProblem { problems: Vec<PathBuf> },
}

#[derive(Deserialize, Default)]
struct EngineConfig {
    #[serde(default)]
    sandbox: IsolateConfig,
    #[serde(default)]
    judge: JudgeConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").with_writer(std::io::stderr).init();
    let cli = Cli::parse();

    let engine: EngineConfig = if cli.config.exists() {
        let text = std::fs::read_to_string(&cli.config)?;
        let mut table: toml::Table = toml::from_str(&text).context("parsing config")?;
        table.retain(|k, _| k == "sandbox" || k == "judge");
        table.try_into().context("reading [sandbox]/[judge] from config")?
    } else {
        EngineConfig::default()
    };
    let languages = Arc::new(LanguageRegistry::load(&cli.languages)?);
    let sandbox = Arc::new(IsolateSandbox::new(engine.sandbox)?);
    sandbox.check().await?;
    let judge = Judge::new(sandbox, languages, engine.judge);

    match cli.command {
        Cmd::Judge { problem, lang, all, json, source } => {
            let problem = Problem::load(&problem)?;
            let lang = match lang {
                Some(l) => l,
                None => language_for(&judge, &source)?,
            };
            let res = run(&judge, &problem, &lang, &source, !all).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                print_result(&res);
            }
            if res.verdict != Verdict::Accepted {
                std::process::exit(1);
            }
        }
        Cmd::CheckProblem { problems } => {
            if problems.is_empty() {
                bail!("give at least one problem directory");
            }
            let mut failures = 0;
            for dir in problems {
                failures += check_problem(&judge, &dir).await?;
            }
            if failures > 0 {
                bail!("{failures} solution(s) did not get the expected verdict");
            }
        }
    }
    Ok(())
}

fn language_for(judge: &Judge, source: &Path) -> Result<String> {
    let ext = source.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let reg = judge.languages();
    let found = reg.ids().find(|id| {
        reg.get(id)
            .and_then(|l| Path::new(&l.source_file).extension()?.to_str().map(|e| e == ext))
            .unwrap_or(false)
    });
    found.map(str::to_string).with_context(|| format!("no language uses the extension {ext:?}; pass --lang"))
}

async fn run(judge: &Judge, problem: &Problem, lang: &str, source: &Path, stop: bool) -> Result<JudgeResult> {
    let code = std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let req = JudgeRequest {
        language: lang,
        source: &code,
        tests: &problem.tests,
        time_limit_ms: problem.time_limit_ms,
        memory_limit_kb: problem.memory_limit_kb,
        comparer: problem.comparer,
        stop_on_first_failure: stop,
        capture_output: false,
        show_failed_input: true,
    };
    Ok(judge.judge(&req, None).await?)
}

fn print_result(res: &JudgeResult) {
    println!("verdict: {}   time: {} ms   memory: {} kB", res.verdict, res.time_ms, res.memory_kb);
    if let Some(out) = &res.compile_output {
        println!("--- compiler ---\n{out}");
    }
    for t in &res.tests {
        let msg = t.message.as_deref().unwrap_or("");
        println!("  {:<12} {:<4} {:>6} ms {:>8} kB  {msg}", t.name, t.verdict.code(), t.time_ms, t.memory_kb);
        if let Some(err) = &t.stderr {
            println!("    stderr: {}", err.lines().last().unwrap_or(""));
        }
    }
    if res.tests.len() < res.tests_total {
        println!("  ({} of {} tests run)", res.tests.len(), res.tests_total);
    }
}

/// Expected verdict from a file name: `ac.py`, `wa-anything.go`, `tle.rs`.
fn expected_verdict(file: &Path) -> Option<Verdict> {
    let stem = file.file_stem()?.to_str()?;
    let label = stem.split(['-', '_', '.']).next()?;
    Verdict::from_code(&label.to_ascii_uppercase())
}

async fn check_problem(judge: &Judge, dir: &Path) -> Result<usize> {
    let problem = Problem::load(dir)?;
    println!("{} ({} tests, version {})", problem.slug, problem.tests.len(), &problem.version[..12]);
    let solutions = dir.join("solutions");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&solutions)
        .with_context(|| format!("{} has no solutions/ directory", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    let mut failures = 0;
    let mut accepted = 0;
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        let Some(want) = expected_verdict(&file) else {
            println!("  SKIP {name}: file name does not start with a verdict code");
            continue;
        };
        let lang = language_for(judge, &file)?;
        let res = run(judge, &problem, &lang, &file, true).await?;
        let ok = res.verdict == want;
        accepted += usize::from(ok && want == Verdict::Accepted);
        failures += usize::from(!ok);
        println!(
            "  {} {name:<24} want {:<3} got {:<3} {:>6} ms {:>8} kB{}",
            if ok { "ok  " } else { "FAIL" },
            want.code(),
            res.verdict.code(),
            res.time_ms,
            res.memory_kb,
            res.failed_test.map(|t| format!("  (first failing test: {t})")).unwrap_or_default(),
        );
        if !ok {
            if let Some(out) = &res.compile_output {
                println!("{out}");
            }
        }
    }
    if accepted == 0 {
        println!("  FAIL no accepted reference solution; the tests are unverified");
        failures += 1;
    }
    Ok(failures)
}

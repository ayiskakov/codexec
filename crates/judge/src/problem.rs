//! Problems on disk.
//!
//! ```text
//! problems/<slug>/
//!   problem.toml        title, limits, comparer, sample test names
//!   statement.md
//!   tests/<name>.in     input, fed to stdin
//!   tests/<name>.out    expected stdout
//!   solutions/          reference and known-bad solutions (used by `codexec check-problem`)
//! ```
//!
//! A problem's `version` is the SHA-256 over its limits, comparer and test
//! data, so every stored verdict names the exact tests that produced it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::compare::Comparer;
use crate::pipeline::TestCase;

#[derive(Debug, thiserror::Error)]
pub enum ProblemError {
    #[error("{path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("problem {slug:?}: {reason}")]
    Invalid { slug: String, reason: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    title: String,
    #[serde(default = "default_difficulty")]
    difficulty: String,
    #[serde(default = "default_time_limit")]
    time_limit_ms: u64,
    #[serde(default = "default_memory_limit")]
    memory_limit_mb: u64,
    #[serde(default)]
    comparer: Comparer,
    /// Names of tests shown in the statement and used by Run.
    #[serde(default)]
    samples: Vec<String>,
    /// Whether the first failing test's input may be shown to the user.
    #[serde(default)]
    show_failed_input: bool,
    #[serde(default)]
    tags: Vec<String>,
}

fn default_difficulty() -> String {
    "medium".into()
}
fn default_time_limit() -> u64 {
    1000
}
fn default_memory_limit() -> u64 {
    256
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub slug: String,
    pub title: String,
    pub difficulty: String,
    pub tags: Vec<String>,
    pub time_limit_ms: u64,
    pub memory_limit_kb: u64,
    pub comparer: Comparer,
    pub show_failed_input: bool,
    pub statement: String,
    pub tests: Vec<TestCase>,
    pub sample_names: Vec<String>,
    /// Content hash of limits + comparer + tests.
    pub version: String,
}

impl Problem {
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, ProblemError> {
        let dir = dir.as_ref();
        let slug = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let invalid = |reason: String| ProblemError::Invalid { slug: slug.clone(), reason };
        if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid("directory name must be lowercase letters, digits and '-'".into()));
        }

        let manifest_path = dir.join("problem.toml");
        let manifest: Manifest = toml::from_str(&read_string(&manifest_path)?)
            .map_err(|source| ProblemError::Parse { path: manifest_path.display().to_string(), source })?;
        let statement = read_string(&dir.join("statement.md")).unwrap_or_default();

        let tests_dir = dir.join("tests");
        let mut names: Vec<String> = std::fs::read_dir(&tests_dir)
            .map_err(|source| ProblemError::Io { path: tests_dir.display().to_string(), source })?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                (p.extension()? == "in").then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        names.sort();
        if names.is_empty() {
            return Err(invalid("no tests/*.in files".into()));
        }

        let mut tests = Vec::with_capacity(names.len());
        for name in &names {
            let input = read_bytes(&tests_dir.join(format!("{name}.in")))?;
            let expected = read_bytes(&tests_dir.join(format!("{name}.out")))
                .map_err(|_| invalid(format!("test {name:?} has no .out file")))?;
            tests.push(TestCase { name: name.clone(), input, expected: Some(expected) });
        }
        for sample in &manifest.samples {
            if !names.contains(sample) {
                return Err(invalid(format!("sample {sample:?} is not a test")));
            }
        }
        if manifest.time_limit_ms == 0 || manifest.memory_limit_mb == 0 {
            return Err(invalid("limits must be positive".into()));
        }

        let mut hasher = Sha256::new();
        hasher.update(format!(
            "v1|{}|{}|{:?}|",
            manifest.time_limit_ms, manifest.memory_limit_mb, manifest.comparer
        ));
        for t in &tests {
            let expected = t.expected.as_deref().unwrap_or_default();
            hasher.update(t.name.as_bytes());
            hasher.update((t.input.len() as u64).to_le_bytes());
            hasher.update(&t.input);
            hasher.update((expected.len() as u64).to_le_bytes());
            hasher.update(expected);
        }
        let version = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();

        Ok(Problem {
            slug,
            title: manifest.title,
            difficulty: manifest.difficulty,
            tags: manifest.tags,
            time_limit_ms: manifest.time_limit_ms,
            memory_limit_kb: manifest.memory_limit_mb * 1024,
            comparer: manifest.comparer,
            show_failed_input: manifest.show_failed_input,
            statement,
            tests,
            sample_names: manifest.samples,
            version,
        })
    }

    pub fn samples(&self) -> Vec<TestCase> {
        self.tests.iter().filter(|t| self.sample_names.contains(&t.name)).cloned().collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProblemSet {
    problems: BTreeMap<String, Arc<Problem>>,
}

impl ProblemSet {
    /// Loads every sub-directory that contains a `problem.toml`.
    pub fn load(root: impl AsRef<Path>) -> Result<Self, ProblemError> {
        let root = root.as_ref();
        let mut problems = BTreeMap::new();
        let entries = std::fs::read_dir(root)
            .map_err(|source| ProblemError::Io { path: root.display().to_string(), source })?;
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            if dir.join("problem.toml").is_file() {
                let problem = Problem::load(&dir)?;
                problems.insert(problem.slug.clone(), Arc::new(problem));
            }
        }
        Ok(Self { problems })
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Problem>> {
        self.problems.get(slug).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Problem>> {
        self.problems.values()
    }

    pub fn len(&self) -> usize {
        self.problems.len()
    }

    pub fn is_empty(&self) -> bool {
        self.problems.is_empty()
    }
}

fn read_string(path: &Path) -> Result<String, ProblemError> {
    std::fs::read_to_string(path)
        .map_err(|source| ProblemError::Io { path: path.display().to_string(), source })
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, ProblemError> {
    std::fs::read(path).map_err(|source| ProblemError::Io { path: path.display().to_string(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("codexec-problem-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn loads_and_versions() {
        let root = scratch("load");
        let dir = root.join("a-plus-b");
        write(&dir, "problem.toml", "title = \"A+B\"\nsamples = [\"01\"]\ncomparer = \"tokens\"\n");
        write(&dir, "statement.md", "Add.");
        write(&dir, "tests/01.in", "1 2\n");
        write(&dir, "tests/01.out", "3\n");
        write(&dir, "tests/02.in", "5 5\n");
        write(&dir, "tests/02.out", "10\n");

        let p = Problem::load(&dir).unwrap();
        assert_eq!(p.slug, "a-plus-b");
        assert_eq!(p.tests.len(), 2);
        assert_eq!(p.samples().len(), 1);
        assert_eq!(p.comparer, Comparer::Tokens);
        assert_eq!(p.memory_limit_kb, 256 * 1024);
        assert_eq!(p.version.len(), 64);

        let v1 = p.version.clone();
        write(&dir, "tests/02.out", "11\n");
        assert_ne!(Problem::load(&dir).unwrap().version, v1, "test change => new version");

        let set = ProblemSet::load(&root).unwrap();
        assert_eq!(set.len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_missing_out_and_bad_sample() {
        let root = scratch("bad");
        let dir = root.join("p1");
        write(&dir, "problem.toml", "title = \"P\"\n");
        write(&dir, "tests/01.in", "1\n");
        assert!(Problem::load(&dir).is_err());
        write(&dir, "tests/01.out", "1\n");
        assert!(Problem::load(&dir).is_ok());
        write(&dir, "problem.toml", "title = \"P\"\nsamples = [\"nope\"]\n");
        assert!(Problem::load(&dir).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

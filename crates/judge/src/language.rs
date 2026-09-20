//! Language registry loaded from `languages.toml`.
//!
//! Adding a language is a config entry, never a code change.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::sandbox::validate_file_name;

#[derive(Debug, thiserror::Error)]
pub enum LanguageError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("cannot parse languages file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("language {id:?}: {reason}")]
    Invalid { id: String, reason: String },
}

/// One command (compile or run) with its environment.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra bind mounts for this step, isolate `--dir` syntax.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Overrides the default process/thread limit.
    #[serde(default)]
    pub processes: Option<u32>,
    /// Overrides isolate's `syscall_flags` for this step (isolate >= 2.7).
    #[serde(default)]
    pub syscall_flags: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Language {
    #[serde(skip)]
    pub id: String,
    pub display_name: String,
    /// File name the submitted source is written to inside the box.
    pub source_file: String,
    pub compile: Option<Step>,
    pub run: Step,
    /// Problem time limits are multiplied by this factor.
    #[serde(default = "one")]
    pub time_multiplier: f64,
    /// Whether the memory limit may be enforced as an address-space rlimit
    /// when cgroups are off. Must be false for Go (and the JVM).
    #[serde(default = "yes")]
    pub address_space_limit: bool,
}

fn one() -> f64 {
    1.0
}
fn yes() -> bool {
    true
}

/// Public view for the API.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageInfo {
    pub id: String,
    pub display_name: String,
    pub compiled: bool,
    pub time_multiplier: f64,
}

#[derive(Debug, Clone, Default)]
pub struct LanguageRegistry {
    languages: BTreeMap<String, Language>,
}

impl LanguageRegistry {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LanguageError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|source| LanguageError::Io { path: path.display().to_string(), source })?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, LanguageError> {
        let mut languages: BTreeMap<String, Language> = toml::from_str(text)?;
        for (id, lang) in languages.iter_mut() {
            lang.id = id.clone();
            let invalid = |reason: String| LanguageError::Invalid { id: id.clone(), reason };
            validate_file_name(&lang.source_file).map_err(|e| invalid(format!("source_file: {e}")))?;
            if lang.run.argv.is_empty() {
                return Err(invalid("run.argv is empty".into()));
            }
            if matches!(&lang.compile, Some(c) if c.argv.is_empty()) {
                return Err(invalid("compile.argv is empty".into()));
            }
            if !(lang.time_multiplier.is_finite() && lang.time_multiplier >= 1.0) {
                return Err(invalid("time_multiplier must be >= 1.0".into()));
            }
        }
        Ok(Self { languages })
    }

    pub fn get(&self, id: &str) -> Option<&Language> {
        self.languages.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.languages.keys().map(String::as_str)
    }

    pub fn infos(&self) -> Vec<LanguageInfo> {
        self.languages
            .values()
            .map(|l| LanguageInfo {
                id: l.id.clone(),
                display_name: l.display_name.clone(),
                compiled: l.compile.is_some(),
                time_multiplier: l.time_multiplier,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.languages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.languages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[python]
display_name = "Python 3"
source_file = "main.py"
time_multiplier = 3.0
[python.run]
argv = ["/usr/bin/python3", "main.py"]

[go]
display_name = "Go"
source_file = "main.go"
address_space_limit = false
[go.compile]
argv = ["/usr/local/go/bin/go", "build", "-o", "main", "main.go"]
env = { GOCACHE = "/gocache" }
dirs = ["/gocache=/var/cache/codexec/gocache"]
syscall_flags = 65531
[go.run]
argv = ["./main"]
env = { GOMAXPROCS = "1" }
"#;

    #[test]
    fn parses_registry() {
        let reg = LanguageRegistry::from_toml(SAMPLE).unwrap();
        assert_eq!(reg.len(), 2);
        let go = reg.get("go").unwrap();
        assert_eq!(go.id, "go");
        assert!(!go.address_space_limit);
        assert_eq!(go.compile.as_ref().unwrap().syscall_flags, Some(65531));
        let py = reg.get("python").unwrap();
        assert!(py.compile.is_none());
        assert_eq!(py.time_multiplier, 3.0);
        assert!(py.address_space_limit);
    }

    #[test]
    fn rejects_bad_entries() {
        let bad_name = SAMPLE.replace("main.py\"\ntime", "../main.py\"\ntime");
        assert!(LanguageRegistry::from_toml(&bad_name).is_err());
        let bad_mult = SAMPLE.replace("time_multiplier = 3.0", "time_multiplier = 0.5");
        assert!(LanguageRegistry::from_toml(&bad_mult).is_err());
        let unknown = SAMPLE.replace("display_name = \"Go\"", "display_name = \"Go\"\nbogus = 1");
        assert!(LanguageRegistry::from_toml(&unknown).is_err());
    }
}

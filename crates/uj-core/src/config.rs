#![allow(clippy::result_large_err)]

use crate::run::Limits;
use crate::verdict::Verdict;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigErrorInfo {
    pub kind: String,
    pub message: String,
    pub hint: Option<String>,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl std::fmt::Display for ConfigErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{path}")?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
                if let Some(col) = self.column {
                    write!(f, ":{col}")?;
                }
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " (hint: {hint})")?;
        }
        Ok(())
    }
}

fn span_to_line_col(
    content: &str,
    span: Option<std::ops::Range<usize>>,
) -> (Option<usize>, Option<usize>) {
    let span = match span {
        Some(s) => s,
        None => return (None, None),
    };
    let mut line = 1;
    let mut col = 1;
    for (idx, ch) in content.char_indices() {
        if idx >= span.start {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (Some(line), Some(col))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProblemType {
    #[default]
    Traditional,
    Interactive,
    Grader,
    Communication,
    OutputOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LimitsConfig {
    pub time: String,
    pub memory: String,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub stderr: Option<String>,
    #[serde(default)]
    pub max_processes: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum IoMode {
    #[default]
    Stdio,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct IoConfig {
    #[serde(default)]
    pub mode: IoMode,
    #[serde(alias = "input")]
    pub input_name: Option<String>,
    #[serde(alias = "output")]
    pub output_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckerType {
    #[default]
    Builtin,
    Testlib,
    Lemon,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct CheckerConfig {
    #[serde(rename = "type", default)]
    pub checker_type: CheckerType,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    #[serde(default = "default_pe_as_wa")]
    pub pe_as_wa: bool,
}

fn default_pe_as_wa() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GraderConfig {
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub headers: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GenConfig {
    pub generator: String,
    pub args: String,
    #[serde(default = "default_count")]
    pub count: usize,
    pub subtask: Option<u32>,
}

fn default_count() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ManualConfig {
    pub input: PathBuf,
    pub subtask: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestsConfig {
    #[serde(default = "default_tests_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_pattern")]
    pub pattern: String,
    #[serde(default)]
    pub samples: Vec<PathBuf>,
    #[serde(rename = "gen", default)]
    pub generators: Vec<GenConfig>,
    #[serde(default)]
    pub manual: Vec<ManualConfig>,
}

fn default_tests_dir() -> PathBuf {
    PathBuf::from("tests")
}

fn default_pattern() -> String {
    "auto".to_string()
}

impl Default for TestsConfig {
    fn default() -> Self {
        Self {
            dir: default_tests_dir(),
            pattern: default_pattern(),
            samples: Vec::new(),
            generators: Vec::new(),
            manual: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScoringType {
    #[default]
    Min,
    Sum,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubtaskConfig {
    pub id: u32,
    pub score: f64,
    #[serde(default)]
    pub scoring: ScoringType,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub depends: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SolutionTag {
    Main,
    Accepted,
    WrongAnswer,
    TimeLimit,
    MemoryLimit,
    RuntimeError,
    Rejected,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SolutionConfig {
    pub path: PathBuf,
    pub tag: SolutionTag,
    #[serde(default)]
    pub expect_subtasks: Option<HashMap<u32, Verdict>>,
}

/// Root problem.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProblemConfig {
    #[serde(default = "default_schema_version")]
    pub schema: u32,
    pub name: String,
    #[serde(rename = "type", default)]
    pub problem_type: ProblemType,
    pub limits: LimitsConfig,
    #[serde(default)]
    pub io: IoConfig,
    #[serde(default)]
    pub checker: CheckerConfig,
    pub interactor: Option<PathBuf>,
    pub grader: Option<GraderConfig>,
    pub manager: Option<PathBuf>,
    pub processes: Option<usize>,
    pub validator: Option<PathBuf>,
    #[serde(default)]
    pub tests: TestsConfig,
    #[serde(default)]
    pub subtasks: Vec<SubtaskConfig>,
    #[serde(default)]
    pub solutions: Vec<SolutionConfig>,
}

fn default_schema_version() -> u32 {
    1
}

impl ProblemConfig {
    /// Parse problem.toml content with line-number aware diagnostics.
    pub fn from_toml_str(content: &str, path: Option<&Path>) -> Result<Self, ConfigErrorInfo> {
        let path_str = path.map(|p| p.to_string_lossy().to_string());
        let config: ProblemConfig = match toml::from_str(content) {
            Ok(c) => c,
            Err(e) => {
                let (line, col) = span_to_line_col(content, e.span());
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Failed to parse problem.toml: {e}"),
                    hint: Some(
                        "Check TOML syntax and required fields (name, limits.time, limits.memory)"
                            .to_string(),
                    ),
                    path: path_str,
                    line,
                    column: col,
                });
            }
        };

        config.validate(path)?;
        Ok(config)
    }

    /// Convert limits to core Limits.
    pub fn to_limits(&self) -> Result<Limits, ConfigErrorInfo> {
        let cpu_ms = parse_time_ms(&self.limits.time).map_err(|msg| ConfigErrorInfo {
            kind: "ConfigError".to_string(),
            message: format!("Invalid time limit '{}': {msg}", self.limits.time),
            hint: Some("Format: '<num>ms' or '<num>s' (e.g. '1s', '500ms')".to_string()),
            path: None,
            line: None,
            column: None,
        })?;

        let mem_bytes = parse_bytes(&self.limits.memory).map_err(|msg| ConfigErrorInfo {
            kind: "ConfigError".to_string(),
            message: format!("Invalid memory limit '{}': {msg}", self.limits.memory),
            hint: Some("Format: '<num>MiB' or '<num>MB' (e.g. '512MiB')".to_string()),
            path: None,
            line: None,
            column: None,
        })?;

        let stack_bytes = match &self.limits.stack {
            Some(s) if s.eq_ignore_ascii_case("same") => None,
            Some(s) if s.eq_ignore_ascii_case("unlimited") => None,
            Some(s) => Some(parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Invalid stack limit '{s}': {msg}"),
                hint: Some("'same', 'unlimited', or '<num>MiB'".to_string()),
                path: None,
                line: None,
                column: None,
            })?),
            None => None,
        };

        let output_bytes = match &self.limits.output {
            Some(s) => parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Invalid output limit '{s}': {msg}"),
                hint: Some("Format: '<num>MiB' or '<num>KiB'".to_string()),
                path: None,
                line: None,
                column: None,
            })?,
            None => Limits::DEFAULT_OUTPUT_BYTES,
        };

        let stderr_bytes = match &self.limits.stderr {
            Some(s) => parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Invalid stderr limit '{s}': {msg}"),
                hint: Some("Format: '<num>KiB'".to_string()),
                path: None,
                line: None,
                column: None,
            })?,
            None => Limits::DEFAULT_STDERR_BYTES,
        };

        let mut lim = Limits::new(cpu_ms, mem_bytes);
        lim.stack_bytes = stack_bytes;
        lim.output_bytes = output_bytes;
        lim.stderr_bytes = stderr_bytes;
        if let Some(p) = self.limits.max_processes {
            lim.max_processes = p;
        }

        Ok(lim)
    }

    /// Validate logical constraints (subtask DAG acyclicity, solution main tag uniqueness).
    pub fn validate(&self, path: Option<&Path>) -> Result<(), ConfigErrorInfo> {
        let path_str = path.map(|p| p.to_string_lossy().to_string());

        // Validate subtasks DAG
        let mut subtask_ids = HashSet::new();
        for sub in &self.subtasks {
            if !subtask_ids.insert(sub.id) {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Duplicate subtask id '{}'", sub.id),
                    hint: Some("Subtask IDs must be unique".to_string()),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
        }

        for sub in &self.subtasks {
            for dep in &sub.depends {
                if !subtask_ids.contains(dep) {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!(
                            "Subtask {} depends on non-existent subtask {dep}",
                            sub.id
                        ),
                        hint: Some("Ensure all dependencies point to defined subtasks".to_string()),
                        path: path_str.clone(),
                        line: None,
                        column: None,
                    });
                }
            }
        }

        // Cycle detection in subtask dependencies
        if let Some(cycle) = detect_subtask_cycle(&self.subtasks) {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Cyclic dependency detected in subtasks: {cycle}"),
                hint: Some(
                    "Subtask dependencies must form a Directed Acyclic Graph (DAG)".to_string(),
                ),
                path: path_str.clone(),
                line: None,
                column: None,
            });
        }

        // Validate solutions (must have at most one 'main')
        let main_count = self
            .solutions
            .iter()
            .filter(|s| s.tag == SolutionTag::Main)
            .count();
        if !self.solutions.is_empty() && main_count > 1 {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Multiple solutions tagged as 'main' (found {main_count})"),
                hint: Some(
                    "Exactly one solution may be tagged as 'main' for generating answers"
                        .to_string(),
                ),
                path: path_str.clone(),
                line: None,
                column: None,
            });
        }

        Ok(())
    }
}

/// Detect cycle in subtask dependencies. Returns cycle string if found.
fn detect_subtask_cycle(subtasks: &[SubtaskConfig]) -> Option<String> {
    let mut adj: HashMap<u32, Vec<u32>> = HashMap::new();
    for sub in subtasks {
        adj.insert(sub.id, sub.depends.clone());
    }

    let mut visited: HashMap<u32, u8> = HashMap::new(); // 0 = unvisited, 1 = visiting, 2 = done
    let mut stack = Vec::new();

    for sub in subtasks {
        if visited.get(&sub.id).copied().unwrap_or(0) == 0
            && dfs_cycle(sub.id, &adj, &mut visited, &mut stack)
        {
            return Some(
                stack
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> "),
            );
        }
    }
    None
}

fn dfs_cycle(
    u: u32,
    adj: &HashMap<u32, Vec<u32>>,
    visited: &mut HashMap<u32, u8>,
    stack: &mut Vec<u32>,
) -> bool {
    visited.insert(u, 1);
    stack.push(u);

    if let Some(deps) = adj.get(&u) {
        for &v in deps {
            match visited.get(&v).copied().unwrap_or(0) {
                1 => {
                    stack.push(v);
                    return true;
                }
                0 => {
                    if dfs_cycle(v, adj, visited, stack) {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }

    visited.insert(u, 2);
    stack.pop();
    false
}

/// Parse time string like "1s", "500ms", "1.5s", "1000" into milliseconds.
pub fn parse_time_ms(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty time string".to_string());
    }

    if let Some(rest) = trimmed
        .strip_suffix("ms")
        .or_else(|| trimmed.strip_suffix("MS"))
    {
        rest.trim()
            .parse::<u64>()
            .map_err(|e| format!("invalid integer ms: {e}"))
    } else if let Some(rest) = trimmed
        .strip_suffix('s')
        .or_else(|| trimmed.strip_suffix('S'))
    {
        let secs = rest
            .trim()
            .parse::<f64>()
            .map_err(|e| format!("invalid float seconds: {e}"))?;
        if secs < 0.0 {
            return Err("time cannot be negative".to_string());
        }
        Ok((secs * 1000.0).round() as u64)
    } else {
        trimmed
            .parse::<u64>()
            .map_err(|e| format!("invalid milliseconds: {e}"))
    }
}

/// Parse memory / byte size string like "512MiB", "512MB", "64KiB", "1GiB", "1024" into bytes.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty byte string".to_string());
    }

    let lower = trimmed.to_lowercase();
    let (num_str, multiplier) = if lower.ends_with("gib") || lower.ends_with("gb") {
        let base = if lower.ends_with("gib") { 3 } else { 2 };
        (&trimmed[..trimmed.len() - base], 1024 * 1024 * 1024)
    } else if lower.ends_with("mib") || lower.ends_with("mb") {
        let base = if lower.ends_with("mib") { 3 } else { 2 };
        (&trimmed[..trimmed.len() - base], 1024 * 1024)
    } else if lower.ends_with("kib") || lower.ends_with("kb") {
        let base = if lower.ends_with("kib") { 3 } else { 2 };
        (&trimmed[..trimmed.len() - base], 1024)
    } else if lower.ends_with('b') {
        (&trimmed[..trimmed.len() - 1], 1)
    } else {
        (trimmed, 1)
    };

    let val = num_str
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("invalid number '{num_str}': {e}"))?;
    if val < 0.0 {
        return Err("byte size cannot be negative".to_string());
    }
    Ok((val * (multiplier as f64)).round() as u64)
}

// ----------------------------------------------------------------------------
// languages.toml configuration
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LanguageConfig {
    pub name: Option<String>,
    pub ext: Vec<String>,
    #[serde(default)]
    pub compile: Option<Vec<String>>,
    #[serde(default)]
    pub compile_windows_extra: Option<Vec<String>>,
    pub run: Vec<String>,
    #[serde(default = "default_time_factor")]
    pub time_factor: f64,
    #[serde(default = "default_max_processes")]
    pub max_processes: u32,
    #[serde(default)]
    pub memory_extra: Option<String>,
    #[serde(default)]
    pub seccomp: Option<String>,
    #[serde(default)]
    pub ro_binds: Option<Vec<PathBuf>>,
}

fn default_time_factor() -> f64 {
    1.0
}

fn default_max_processes() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct LanguagesConfig {
    #[serde(rename = "lang", default)]
    pub languages: HashMap<String, LanguageConfig>,
}

impl LanguagesConfig {
    pub fn from_toml_str(content: &str, path: Option<&Path>) -> Result<Self, ConfigErrorInfo> {
        let path_str = path.map(|p| p.to_string_lossy().to_string());
        toml::from_str(content).map_err(|e| {
            let (line, col) = span_to_line_col(content, e.span());
            ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!("Failed to parse languages.toml: {e}"),
                hint: Some("Check language configuration format".to_string()),
                path: path_str,
                line,
                column: col,
            }
        })
    }

    /// Find language config matching a given file extension.
    pub fn find_by_extension(&self, ext: &str) -> Option<(&str, &LanguageConfig)> {
        let clean_ext = ext.trim_start_matches('.').to_lowercase();
        for (id, lang) in &self.languages {
            if lang.ext.iter().any(|e| e.eq_ignore_ascii_case(&clean_ext)) {
                return Some((id.as_str(), lang));
            }
        }
        None
    }

    /// Merge higher-priority config on top of self (overlaying entries).
    pub fn merge_overlay(&mut self, other: LanguagesConfig) {
        for (id, lang) in other.languages {
            self.languages.insert(id, lang);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time_ms() {
        assert_eq!(parse_time_ms("1s").unwrap(), 1000);
        assert_eq!(parse_time_ms("1.5s").unwrap(), 1500);
        assert_eq!(parse_time_ms("500ms").unwrap(), 500);
        assert_eq!(parse_time_ms("2000").unwrap(), 2000);
    }

    #[test]
    fn test_parse_bytes() {
        assert_eq!(parse_bytes("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_bytes("64KiB").unwrap(), 64 * 1024);
        assert_eq!(parse_bytes("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
    }

    #[test]
    fn test_problem_toml_parsing() {
        let sample = r#"
schema = 1
name = "aplusb"
type = "traditional"

[limits]
time = "1s"
memory = "512MiB"
stack = "same"
output = "64MiB"

[io]
mode = "stdio"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 1
score = 30.0
scoring = "min"
tests = ["1-10"]

[[subtasks]]
id = 2
score = 70.0
scoring = "min"
depends = [1]

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
        let config = ProblemConfig::from_toml_str(sample, None).unwrap();
        assert_eq!(config.name, "aplusb");
        assert_eq!(config.limits.time, "1s");
        let limits = config.to_limits().unwrap();
        assert_eq!(limits.cpu_time_ms, 1000);
        assert_eq!(limits.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.stack_bytes, None);
        assert_eq!(config.subtasks.len(), 2);
    }

    #[test]
    fn test_subtask_cycle_detection() {
        let sample = r#"
name = "cycle"
[limits]
time = "1s"
memory = "256MiB"

[[subtasks]]
id = 1
score = 50.0
depends = [2]

[[subtasks]]
id = 2
score = 50.0
depends = [1]
"#;
        let res = ProblemConfig::from_toml_str(sample, None);
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.message.contains("Cyclic dependency"));
    }
}

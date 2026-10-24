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
    #[serde(alias = "ac")]
    Accepted,
    #[serde(alias = "wa")]
    WrongAnswer,
    #[serde(alias = "tle")]
    TimeLimit,
    #[serde(alias = "mle")]
    MemoryLimit,
    #[serde(alias = "re")]
    RuntimeError,
    #[serde(alias = "wa_or_tle", alias = "tle_or_wa")]
    WrongAnswerOrTimeLimit,
    Rejected,
    Partial,
}

impl SolutionTag {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Accepted => "accepted",
            Self::WrongAnswer => "wrong_answer",
            Self::TimeLimit => "time_limit",
            Self::MemoryLimit => "memory_limit",
            Self::RuntimeError => "runtime_error",
            Self::WrongAnswerOrTimeLimit => "wa_or_tle",
            Self::Rejected => "rejected",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SolutionConfig {
    pub path: PathBuf,
    pub tag: SolutionTag,
    #[serde(default)]
    pub expect_subtasks: Option<HashMap<u32, Verdict>>,
    #[serde(default)]
    pub allow_verdicts: Option<Vec<Verdict>>,
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
    pub communication_topology: Option<String>,
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

    /// Calculate the total maximum score for the problem across all subtasks.
    pub fn max_score(&self) -> f64 {
        if self.subtasks.is_empty() {
            100.0
        } else {
            self.subtasks.iter().map(|s| s.score).sum()
        }
    }

    /// Convert limits to core Limits.
    pub fn to_limits(&self) -> Result<Limits, ConfigErrorInfo> {
        const MAX_CPU_TIME_MS: u64 = 3_600_000; // 1 hour
        const MAX_MEMORY_BYTES: u64 = 64 * 1024 * 1024 * 1024; // 64 GiB
        const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
        const MAX_STDERR_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

        let cpu_ms = parse_time_ms(&self.limits.time).map_err(|msg| ConfigErrorInfo {
            kind: "ConfigError".to_string(),
            message: format!("Invalid time limit '{}': {msg}", self.limits.time),
            hint: Some("Format: '<num>ms' or '<num>s' (e.g. '1s', '500ms')".to_string()),
            path: None,
            line: None,
            column: None,
        })?;
        if cpu_ms == 0 || cpu_ms > MAX_CPU_TIME_MS {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!(
                    "Invalid time limit '{}': must be between 1ms and 3600s (got {cpu_ms}ms)",
                    self.limits.time
                ),
                hint: Some("Specify a positive time limit up to 3600s (e.g. '1s')".to_string()),
                path: None,
                line: None,
                column: None,
            });
        }

        let mem_bytes = parse_bytes(&self.limits.memory).map_err(|msg| ConfigErrorInfo {
            kind: "ConfigError".to_string(),
            message: format!("Invalid memory limit '{}': {msg}", self.limits.memory),
            hint: Some("Format: '<num>MiB' or '<num>MB' (e.g. '512MiB')".to_string()),
            path: None,
            line: None,
            column: None,
        })?;
        if mem_bytes == 0 || mem_bytes > MAX_MEMORY_BYTES {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: format!(
                    "Invalid memory limit '{}': must be > 0 and <= 64GiB",
                    self.limits.memory
                ),
                hint: Some("Specify a positive memory limit (e.g. '256MiB')".to_string()),
                path: None,
                line: None,
                column: None,
            });
        }

        let stack_bytes = match &self.limits.stack {
            Some(s) if s.eq_ignore_ascii_case("same") => None,
            Some(s) if s.eq_ignore_ascii_case("unlimited") => None,
            Some(s) => {
                let b = parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Invalid stack limit '{s}': {msg}"),
                    hint: Some("'same', 'unlimited', or '<num>MiB'".to_string()),
                    path: None,
                    line: None,
                    column: None,
                })?;
                if b == 0 || b > MAX_MEMORY_BYTES {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!("Invalid stack limit '{s}': must be > 0 and <= 64GiB"),
                        hint: Some("'same', 'unlimited', or '<num>MiB'".to_string()),
                        path: None,
                        line: None,
                        column: None,
                    });
                }
                Some(b)
            }
            None => None,
        };

        let output_bytes = match &self.limits.output {
            Some(s) => {
                let b = parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Invalid output limit '{s}': {msg}"),
                    hint: Some("Format: '<num>MiB' or '<num>KiB'".to_string()),
                    path: None,
                    line: None,
                    column: None,
                })?;
                if b == 0 || b > MAX_OUTPUT_BYTES {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!("Invalid output limit '{s}': must be > 0 and <= 4GiB"),
                        hint: Some("Format: '<num>MiB' or '<num>KiB'".to_string()),
                        path: None,
                        line: None,
                        column: None,
                    });
                }
                b
            }
            None => Limits::DEFAULT_OUTPUT_BYTES,
        };

        let stderr_bytes = match &self.limits.stderr {
            Some(s) => {
                let b = parse_bytes(s).map_err(|msg| ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Invalid stderr limit '{s}': {msg}"),
                    hint: Some("Format: '<num>KiB'".to_string()),
                    path: None,
                    line: None,
                    column: None,
                })?;
                if b == 0 || b > MAX_STDERR_BYTES {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!("Invalid stderr limit '{s}': must be > 0 and <= 256MiB"),
                        hint: Some("Format: '<num>KiB'".to_string()),
                        path: None,
                        line: None,
                        column: None,
                    });
                }
                b
            }
            None => Limits::DEFAULT_STDERR_BYTES,
        };

        let mut lim = Limits::new(cpu_ms, mem_bytes);
        lim.stack_bytes = stack_bytes;
        lim.output_bytes = output_bytes;
        lim.stderr_bytes = stderr_bytes;
        if let Some(p) = self.limits.max_processes {
            if p == 0 {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: "Invalid max_processes '0': must be >= 1".to_string(),
                    hint: Some("Specify `max_processes = 1` or higher".to_string()),
                    path: None,
                    line: None,
                    column: None,
                });
            }
            lim.max_processes = p;
        }

        Ok(lim)
    }

    /// Validate logical constraints (subtask DAG acyclicity, solution main tag uniqueness, non-empty names, finite scores).
    pub fn validate(&self, path: Option<&Path>) -> Result<(), ConfigErrorInfo> {
        let path_str = path.map(|p| p.to_string_lossy().to_string());

        // Validate problem name
        if self.name.trim().is_empty() {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: "Problem 'name' must not be empty".to_string(),
                hint: Some("Specify `name = \"problem_name\"` in problem.toml".to_string()),
                path: path_str.clone(),
                line: None,
                column: None,
            });
        }

        // Validate problem type requirements
        if self.problem_type == ProblemType::Interactive && self.interactor.is_none() {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: "Interactive problem requires an 'interactor' file".to_string(),
                hint: Some("Specify `interactor = \"interactor.cpp\"` in problem.toml".to_string()),
                path: path_str.clone(),
                line: None,
                column: None,
            });
        }
        if self.problem_type == ProblemType::Grader && self.grader.is_none() {
            return Err(ConfigErrorInfo {
                kind: "ConfigError".to_string(),
                message: "Grader problem requires a [grader] section".to_string(),
                hint: Some("Specify `[grader]` in problem.toml".to_string()),
                path: path_str.clone(),
                line: None,
                column: None,
            });
        }
        if self.problem_type == ProblemType::Communication {
            if self.manager.is_none() {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: "Communication problem requires a 'manager' file".to_string(),
                    hint: Some("Specify `manager = \"manager.cpp\"` in problem.toml".to_string()),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
            let procs = self.processes.unwrap_or(1);
            if procs > 1 {
                let topo = self.communication_topology.as_deref().unwrap_or("");
                if topo != "ring" && topo != "pipeline" && topo != "star" {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!(
                            "Multi-participant communication (processes = {procs}) requires explicit supported topology (`communication_topology = \"ring\"`, \"pipeline\", or \"star\")."
                        ),
                        hint: Some(
                            "Add `communication_topology = \"ring\"` or `\"star\"` to problem.toml"
                                .to_string(),
                        ),
                        path: path_str.clone(),
                        line: None,
                        column: None,
                    });
                }
            }
        }

        // Validate limits early at config load/validation time
        self.to_limits().map_err(|mut e| {
            if e.path.is_none() {
                e.path = path_str.clone();
            }
            e
        })?;

        // Path safety check (prevent escaping problem directory via .. or absolute paths)
        let check_path_safe = |p: &Path, field: &str| -> Result<(), ConfigErrorInfo> {
            if p.as_os_str().is_empty() {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!("Field '{field}' must not be empty"),
                    hint: Some(
                        "Specify a valid relative path inside the problem directory".to_string(),
                    ),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
            for comp in p.components() {
                if matches!(
                    comp,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                ) {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!(
                            "Field '{field}' contains forbidden parent reference '..' or absolute path: {}",
                            p.display()
                        ),
                        hint: Some(
                            "File paths must remain inside the problem directory".to_string(),
                        ),
                        path: path_str.clone(),
                        line: None,
                        column: None,
                    });
                }
            }
            Ok(())
        };

        // Single-component filename check for io.input_name / io.output_name
        let check_single_filename = |name: &str, field: &str| -> Result<(), ConfigErrorInfo> {
            let trimmed = name.trim();
            if trimmed.is_empty()
                || trimmed.contains('/')
                || trimmed.contains('\\')
                || trimmed == "."
                || trimmed == ".."
            {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!(
                        "Field '{field}' contains forbidden path traversal or separator: '{name}'"
                    ),
                    hint: Some("Specify a single plain filename such as \"input.txt\"".to_string()),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
            let p = Path::new(trimmed);
            let mut comps = p.components();
            match (comps.next(), comps.next()) {
                (Some(std::path::Component::Normal(_)), None) => Ok(()),
                _ => Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!(
                        "Field '{field}' must be a single relative filename component: '{name}'"
                    ),
                    hint: Some("Specify a single plain filename such as \"input.txt\"".to_string()),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                }),
            }
        };

        if let Some(ref in_name) = self.io.input_name {
            check_single_filename(in_name, "io.input_name")?;
        }
        if let Some(ref out_name) = self.io.output_name {
            check_single_filename(out_name, "io.output_name")?;
        }
        check_path_safe(&self.tests.dir, "tests.dir")?;
        if let Some(ref p) = self.interactor {
            check_path_safe(p, "interactor")?;
        }
        if let Some(ref p) = self.manager {
            check_path_safe(p, "manager")?;
        }
        if let Some(ref p) = self.validator {
            check_path_safe(p, "validator")?;
        }
        if let Some(ref p) = self.checker.path {
            check_path_safe(p, "checker.path")?;
        }
        for p in &self.tests.samples {
            check_path_safe(p, "tests.samples")?;
        }
        for m in &self.tests.manual {
            check_path_safe(&m.input, "tests.manual.input")?;
        }
        for g in &self.tests.generators {
            check_path_safe(Path::new(&g.generator), "tests.gen.generator")?;
        }
        if let Some(ref grader) = self.grader {
            for f in &grader.files {
                check_path_safe(f, "grader.files")?;
            }
            for h in &grader.headers {
                check_path_safe(h, "grader.headers")?;
            }
        }
        for sol in &self.solutions {
            check_path_safe(&sol.path, "solutions.path")?;
        }

        // Validate subtasks DAG and scores
        let mut subtask_ids = HashSet::new();
        for sub in &self.subtasks {
            if !sub.score.is_finite() || sub.score < 0.0 {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!(
                        "Subtask {} has invalid score '{}': score must be a non-negative finite number",
                        sub.id, sub.score
                    ),
                    hint: Some(
                        "Specify a non-negative numeric score, e.g. score = 100.0".to_string(),
                    ),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
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
            if sub.tests.is_empty() {
                return Err(ConfigErrorInfo {
                    kind: "ConfigError".to_string(),
                    message: format!(
                        "Subtask {} has no test patterns specified ('tests' must not be empty)",
                        sub.id
                    ),
                    hint: Some(
                        "Specify at least one test pattern, e.g. `tests = [\"1-10\"]`".to_string(),
                    ),
                    path: path_str.clone(),
                    line: None,
                    column: None,
                });
            }
            for pat in &sub.tests {
                if let Err(err_msg) = crate::tests::validate_subtask_pattern(pat) {
                    return Err(ConfigErrorInfo {
                        kind: "ConfigError".to_string(),
                        message: format!("Subtask {} has {err_msg}", sub.id),
                        hint: Some(
                            "Valid patterns: numeric range '1-10' (start <= end), glob 'sample*', regex 're:^1_', or exact name".to_string(),
                        ),
                        path: path_str.clone(),
                        line: None,
                        column: None,
                    });
                }
            }
        }

        // Reject static pattern overlaps across subtasks (identical patterns or overlapping numeric ranges/stems)
        if self.subtasks.len() > 1 {
            let parse_num_interval = |pat: &str| -> Option<(u64, u64)> {
                let p = pat.trim();
                if let Ok(v) = p.parse::<u64>() {
                    return Some((v, v));
                }
                if let Some((s, e)) = p.split_once('-')
                    && let (Ok(start), Ok(end)) = (s.trim().parse::<u64>(), e.trim().parse::<u64>())
                    && start <= end
                {
                    return Some((start, end));
                }
                None
            };

            for (i, sub_a) in self.subtasks.iter().enumerate() {
                for sub_b in &self.subtasks[i + 1..] {
                    for pat_a in &sub_a.tests {
                        let trim_a = pat_a.trim();
                        let range_a = parse_num_interval(trim_a);
                        for pat_b in &sub_b.tests {
                            let trim_b = pat_b.trim();
                            let range_b = parse_num_interval(trim_b);
                            let overlaps = if trim_a == trim_b {
                                true
                            } else if let (Some((sa, ea)), Some((sb, eb))) = (range_a, range_b) {
                                sa <= eb && sb <= ea
                            } else {
                                false
                            };
                            if overlaps {
                                return Err(ConfigErrorInfo {
                                    kind: "ConfigError".to_string(),
                                    message: format!(
                                        "Subtask {} pattern '{}' overlaps with subtask {} pattern '{}'",
                                        sub_a.id, trim_a, sub_b.id, trim_b
                                    ),
                                    hint: Some(
                                        "Each test case must belong to a single subtask; use `depends = [...]` to model subtask dependencies".to_string(),
                                    ),
                                    path: path_str.clone(),
                                    line: None,
                                    column: None,
                                });
                            }
                        }
                    }
                }
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
                0 if dfs_cycle(v, adj, visited, stack) => {
                    return true;
                }
                _ => {}
            }
        }
    }

    visited.insert(u, 2);
    stack.pop();
    false
}

/// Strip an ASCII suffix case-insensitively without allocating or slicing across multi-byte UTF-8 chars.
fn strip_ascii_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() >= suffix.len() && s.is_char_boundary(s.len() - suffix.len()) {
        let (head, tail) = s.split_at(s.len() - suffix.len());
        if tail.eq_ignore_ascii_case(suffix) {
            return Some(head);
        }
    }
    None
}

/// Parse time string like "1s", "500ms", "1.5s", "1000" into milliseconds.
pub fn parse_time_ms(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty time string".to_string());
    }

    if let Some(rest) = strip_ascii_suffix_ci(trimmed, "ms") {
        rest.trim()
            .parse::<u64>()
            .map_err(|e| format!("invalid integer ms: {e}"))
    } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "s") {
        let secs = rest
            .trim()
            .parse::<f64>()
            .map_err(|e| format!("invalid float seconds: {e}"))?;
        if !secs.is_finite() || secs < 0.0 {
            return Err("time must be a non-negative finite number".to_string());
        }
        let ms = (secs * 1000.0).round();
        if !ms.is_finite() || ms >= (u64::MAX as f64) {
            return Err("time limit overflows u64 milliseconds".to_string());
        }
        Ok(ms as u64)
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

    let (num_str, multiplier): (&str, u64) =
        if let Some(rest) = strip_ascii_suffix_ci(trimmed, "gib") {
            (rest, 1024 * 1024 * 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "gb") {
            (rest, 1024 * 1024 * 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "mib") {
            (rest, 1024 * 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "mb") {
            (rest, 1024 * 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "kib") {
            (rest, 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "kb") {
            (rest, 1024)
        } else if let Some(rest) = strip_ascii_suffix_ci(trimmed, "b") {
            (rest, 1)
        } else {
            (trimmed, 1)
        };

    let val = num_str
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("invalid number '{num_str}': {e}"))?;
    if !val.is_finite() || val < 0.0 {
        return Err("byte size must be a non-negative finite number".to_string());
    }
    let scaled = (val * (multiplier as f64)).round();
    if !scaled.is_finite() || scaled >= (u64::MAX as f64) {
        return Err("byte size overflows u64".to_string());
    }
    Ok(scaled as u64)
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

    /// Find language config matching a given file extension deterministically.
    pub fn find_by_extension(&self, ext: &str) -> Option<(&str, &LanguageConfig)> {
        let clean_ext = ext.trim_start_matches('.').to_lowercase();
        let mut entries: Vec<(&String, &LanguageConfig)> = self.languages.iter().collect();
        entries.sort_by(
            |(id_a, _), (id_b, _)| match (id_a.as_str(), id_b.as_str()) {
                ("cpp17", _) => std::cmp::Ordering::Less,
                (_, "cpp17") => std::cmp::Ordering::Greater,
                _ => id_a.cmp(id_b),
            },
        );
        for (id, lang) in entries {
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
tests = ["11-50"]
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
tests = ["1"]
depends = [2]

[[subtasks]]
id = 2
score = 50.0
tests = ["2"]
depends = [1]
"#;
        let res = ProblemConfig::from_toml_str(sample, None);
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.message.contains("Cyclic dependency"));
    }

    #[test]
    fn test_problem_validation_empty_name() {
        let sample = r#"
name = "   "
[limits]
time = "1s"
memory = "256MiB"
"#;
        let res = ProblemConfig::from_toml_str(sample, None);
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .message
                .contains("Problem 'name' must not be empty")
        );
    }

    #[test]
    fn test_problem_validation_negative_or_nan_score() {
        let sample_neg = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[[subtasks]]
id = 1
score = -10.0
tests = ["1"]
"#;
        let res_neg = ProblemConfig::from_toml_str(sample_neg, None);
        assert!(res_neg.is_err());
        assert!(
            res_neg
                .unwrap_err()
                .message
                .contains("score must be a non-negative finite number")
        );
    }

    #[test]
    fn test_problem_validation_interactive_requires_interactor() {
        let sample = r#"
name = "interactive_prob"
type = "interactive"
[limits]
time = "1s"
memory = "256MiB"
"#;
        let res = ProblemConfig::from_toml_str(sample, None);
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .message
                .contains("requires an 'interactor' file")
        );
    }

    #[test]
    fn test_problem_validation_path_traversal() {
        let cases = [
            (
                "interactor",
                r#"
name = "prob"
type = "traditional"
interactor = "../escape.cpp"
[limits]
time = "1s"
memory = "256MiB"
"#,
            ),
            (
                "checker.path",
                r#"
name = "prob"
type = "traditional"
[limits]
time = "1s"
memory = "256MiB"
[checker]
type = "testlib"
path = "../../outside_checker.cpp"
"#,
            ),
            (
                "tests.gen.generator",
                r#"
name = "prob"
type = "traditional"
[limits]
time = "1s"
memory = "256MiB"
[[tests.gen]]
generator = "../evil_gen.cpp"
args = "1"
"#,
            ),
            (
                "tests.manual.input",
                r#"
name = "prob"
type = "traditional"
[limits]
time = "1s"
memory = "256MiB"
[[tests.manual]]
input = "../secret.txt"
"#,
            ),
            (
                "grader.files",
                r#"
name = "prob"
type = "grader"
[limits]
time = "1s"
memory = "256MiB"
[grader]
files = ["../outside_grader.cpp"]
"#,
            ),
        ];

        for (field, sample) in cases {
            let res = ProblemConfig::from_toml_str(sample, None);
            assert!(res.is_err(), "Expected path traversal error for {field}");
            let msg = res.unwrap_err().message;
            assert!(
                msg.contains("forbidden parent reference '..'"),
                "Unexpected error for {field}: {msg}"
            );
        }
    }

    #[test]
    fn test_star_topology_config_allowed() {
        let sample = r#"
name = "comm_star"
type = "communication"
manager = "manager.cpp"
processes = 2
communication_topology = "star"
[limits]
time = "1s"
memory = "256MiB"
"#;
        let cfg = ProblemConfig::from_toml_str(sample, None).unwrap();
        assert_eq!(cfg.communication_topology.as_deref(), Some("star"));
        assert_eq!(cfg.processes, Some(2));
    }

    #[test]
    fn test_parse_time_and_bytes_nan() {
        assert!(parse_time_ms("NaNs").is_err());
        assert!(parse_time_ms("infs").is_err());
        assert!(parse_time_ms("-1s").is_err());

        assert!(parse_bytes("NaNMiB").is_err());
        assert!(parse_bytes("infGiB").is_err());
        assert!(parse_bytes("-512MiB").is_err());
    }

    #[test]
    fn test_solution_tag_aliases_and_allow_verdicts() {
        let sample = r#"
name = "alias_test"
[limits]
time = "1s"
memory = "256MiB"

[[solutions]]
path = "solutions/std.cpp"
tag = "main"

[[solutions]]
path = "solutions/ac2.cpp"
tag = "ac"

[[solutions]]
path = "solutions/wa.cpp"
tag = "wa"

[[solutions]]
path = "solutions/tle.cpp"
tag = "tle"

[[solutions]]
path = "solutions/mle.cpp"
tag = "mle"

[[solutions]]
path = "solutions/re.cpp"
tag = "re"

[[solutions]]
path = "solutions/wa_tle.cpp"
tag = "wa_or_tle"

[[solutions]]
path = "solutions/wa_custom.cpp"
tag = "wa"
allow_verdicts = ["WA", "TLE"]
"#;
        let cfg = ProblemConfig::from_toml_str(sample, None).unwrap();
        assert_eq!(cfg.solutions.len(), 8);
        assert_eq!(cfg.solutions[0].tag, SolutionTag::Main);
        assert_eq!(cfg.solutions[1].tag, SolutionTag::Accepted);
        assert_eq!(cfg.solutions[2].tag, SolutionTag::WrongAnswer);
        assert_eq!(cfg.solutions[3].tag, SolutionTag::TimeLimit);
        assert_eq!(cfg.solutions[4].tag, SolutionTag::MemoryLimit);
        assert_eq!(cfg.solutions[5].tag, SolutionTag::RuntimeError);
        assert_eq!(cfg.solutions[6].tag, SolutionTag::WrongAnswerOrTimeLimit);
        assert_eq!(cfg.solutions[7].tag, SolutionTag::WrongAnswer);
        assert_eq!(
            cfg.solutions[7].allow_verdicts,
            Some(vec![Verdict::WrongAnswer, Verdict::TimeLimitExceeded])
        );

        // Verify serialization outputs canonical snake_case names
        assert_eq!(
            serde_json::to_string(&SolutionTag::Accepted).unwrap(),
            "\"accepted\""
        );
        assert_eq!(
            serde_json::to_string(&SolutionTag::WrongAnswer).unwrap(),
            "\"wrong_answer\""
        );
        assert_eq!(
            serde_json::to_string(&SolutionTag::TimeLimit).unwrap(),
            "\"time_limit\""
        );
        assert_eq!(
            serde_json::to_string(&SolutionTag::MemoryLimit).unwrap(),
            "\"memory_limit\""
        );
        assert_eq!(
            serde_json::to_string(&SolutionTag::RuntimeError).unwrap(),
            "\"runtime_error\""
        );
        assert_eq!(
            serde_json::to_string(&SolutionTag::WrongAnswerOrTimeLimit).unwrap(),
            "\"wrong_answer_or_time_limit\""
        );
    }

    #[test]
    fn test_parse_bytes_unicode_no_panic() {
        // U+212A (Kelvin sign) lowercases to 'k' (1 byte vs 3 bytes in UTF-8).
        assert!(parse_bytes("1\u{212A}B").is_err());
        // U+0130 ('İ') lowercases to "i\u{307}" (2 bytes vs 3 bytes in UTF-8).
        assert!(parse_bytes("1\u{0130}B").is_err());
        assert!(parse_time_ms("1\u{212A}s").is_err());
    }

    #[test]
    fn test_limits_validation_in_from_toml_str() {
        // Zero time limit must be rejected during config validation
        let zero_time = r#"
name = "prob"
[limits]
time = "0s"
memory = "256MiB"
"#;
        assert!(ProblemConfig::from_toml_str(zero_time, None).is_err());

        // Huge time limit must be rejected
        let huge_time = r#"
name = "prob"
[limits]
time = "1e300s"
memory = "256MiB"
"#;
        assert!(ProblemConfig::from_toml_str(huge_time, None).is_err());

        // Zero memory limit must be rejected
        let zero_mem = r#"
name = "prob"
[limits]
time = "1s"
memory = "0MiB"
"#;
        assert!(ProblemConfig::from_toml_str(zero_mem, None).is_err());

        // Huge memory limit (> 64GiB) must be rejected
        let huge_mem = r#"
name = "prob"
[limits]
time = "1s"
memory = "128GiB"
"#;
        assert!(ProblemConfig::from_toml_str(huge_mem, None).is_err());
    }

    #[test]
    fn test_io_filename_and_tests_dir_validation() {
        let bad_input_name = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[io]
mode = "file"
input_name = "../secret.in"
output_name = "out.txt"
"#;
        let err = ProblemConfig::from_toml_str(bad_input_name, None).unwrap_err();
        assert!(err.message.contains("io.input_name"));

        let bad_output_subdir = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[io]
mode = "file"
input_name = "in.txt"
output_name = "sub/out.txt"
"#;
        let err = ProblemConfig::from_toml_str(bad_output_subdir, None).unwrap_err();
        assert!(err.message.contains("io.output_name"));

        let bad_tests_dir = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[tests]
dir = "../../etc"
"#;
        let err = ProblemConfig::from_toml_str(bad_tests_dir, None).unwrap_err();
        assert!(err.message.contains("tests.dir"));
    }

    #[test]
    fn test_subtask_pattern_and_overlap_validation() {
        // Empty tests list
        let empty_tests = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[[subtasks]]
id = 1
score = 100.0
tests = []
"#;
        let err = ProblemConfig::from_toml_str(empty_tests, None).unwrap_err();
        assert!(err.message.contains("no test patterns specified"));

        // Invalid regex pattern
        let bad_regex = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[[subtasks]]
id = 1
score = 100.0
tests = ["re:[a-z"]
"#;
        let err = ProblemConfig::from_toml_str(bad_regex, None).unwrap_err();
        assert!(err.message.contains("invalid regex"));

        // Reversed range
        let reversed_range = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[[subtasks]]
id = 1
score = 100.0
tests = ["10-1"]
"#;
        let err = ProblemConfig::from_toml_str(reversed_range, None).unwrap_err();
        assert!(err.message.contains("start (10) > end (1)"));

        // Overlapping numeric ranges across subtasks
        let overlap_ranges = r#"
name = "prob"
[limits]
time = "1s"
memory = "256MiB"
[[subtasks]]
id = 1
score = 50.0
tests = ["1-5"]
[[subtasks]]
id = 2
score = 50.0
tests = ["5-10"]
"#;
        let err = ProblemConfig::from_toml_str(overlap_ranges, None).unwrap_err();
        assert!(err.message.contains("overlaps with subtask"));
    }
}

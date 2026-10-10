use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uj_core::verdict::Verdict;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub nt_status: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct DiffSummary {
    pub line: usize,
    pub column: usize,
    pub token_index: usize,
    pub expected: String,
    pub found: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct TimingInfo {
    pub rejudged: bool,
    pub confidence: String,
    #[serde(default)]
    pub samples_ms: Vec<u64>,
}

/// Single test result matching Protocol specification 4.3.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SingleTestResult {
    pub id: String,
    pub subtask: Option<u32>,
    pub verdict: Verdict,
    pub score: f64,
    pub time_ms: u64,
    pub wall_ms: u64,
    pub memory_kb: u64,
    pub exit: ExitInfo,
    pub checker: Option<String>,
    pub diff: Option<DiffSummary>,
    pub timing: TimingInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubtaskResult {
    pub id: u32,
    pub score: f64,
    pub max_score: f64,
    pub verdict: Verdict,
    pub test_ids: Vec<String>,
}

/// Submission result schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubmissionJudgeReport {
    pub schema_version: u32,
    pub problem: String,
    pub submission: String,
    pub verdict: Verdict,
    pub score: f64,
    pub max_time_ms: u64,
    pub max_memory_kb: u64,
    pub subtasks: Vec<SubtaskResult>,
    pub tests: Vec<SingleTestResult>,
}

/// NDJSON event definitions.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum JudgeEvent {
    CompileStart {
        target: String,
        compiler: String,
    },
    CompileDone {
        target: String,
        success: bool,
        duration_ms: u64,
        error_output: Option<String>,
    },
    TestStart {
        id: String,
        subtask: Option<u32>,
    },
    TestDone(SingleTestResult),
    SubtaskDone(SubtaskResult),
    SubmissionDone(SubmissionJudgeReport),
    Warning {
        message: String,
        context: Option<String>,
    },
    Summary {
        total_time_ms: u64,
        peak_memory_kb: u64,
        final_verdict: Verdict,
        final_score: f64,
    },
}

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uj_core::verdict::Verdict;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub nt_status: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct DiffSummary {
    pub line: usize,
    pub column: usize,
    pub token_index: usize,
    pub expected: String,
    pub found: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct TimingInfo {
    pub rejudged: bool,
    pub confidence: String,
    #[serde(default)]
    pub samples_ms: Vec<u64>,
}

/// Single test result matching Protocol specification 4.3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SingleTestResult {
    pub id: String,
    pub subtask: Option<u32>,
    pub verdict: Verdict,
    pub score: f64,
    pub time_ms: u64,
    pub wall_ms: u64,
    pub memory_kb: u64,
    pub exit: ExitInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub checker: Option<String>,
    pub diff: Option<DiffSummary>,
    pub timing: TimingInfo,
    #[serde(default)]
    pub sandbox_tier: Option<uj_core::run::Tier>,
}

impl SingleTestResult {
    /// Create a synthetic test result (e.g. for failed worker init, authorization failure, or fail-fast skip).
    pub fn synthetic(
        test: &uj_core::tests::TestCase,
        verdict: Verdict,
        message: Option<String>,
        sandbox_tier: Option<uj_core::run::Tier>,
    ) -> Self {
        Self {
            id: test.name.clone(),
            subtask: test.subtask_id,
            verdict,
            score: 0.0,
            time_ms: 0,
            wall_ms: 0,
            memory_kb: 0,
            exit: Default::default(),
            message,
            checker: None,
            diff: None,
            timing: Default::default(),
            sandbox_tier,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubtaskResult {
    pub id: u32,
    pub score: f64,
    pub max_score: f64,
    pub verdict: Verdict,
    pub test_ids: Vec<String>,
}

/// Submission result schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtask: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot: Option<usize>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubmissionRef {
    pub id: usize,
    pub problem: String,
    pub name: String,
}

/// Multi-submission NDJSON single line: {"submission":{..},"event":"test_done","data":{..}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PoolEvent {
    pub submission: SubmissionRef,
    #[serde(flatten)]
    pub inner: JudgeEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_start_slot_optional() {
        // 1. Deserializing old JSON without slot should succeed with slot: None
        let old_json = r#"{"event":"test_start","data":{"id":"test_1","subtask":1}}"#;
        let event: JudgeEvent = serde_json::from_str(old_json).unwrap();
        match event {
            JudgeEvent::TestStart { id, subtask, slot } => {
                assert_eq!(id, "test_1");
                assert_eq!(subtask, Some(1));
                assert_eq!(slot, None);
            }
            _ => panic!("Expected TestStart"),
        }

        // 2. Serializing with slot: None should not output "slot"
        let ev_none = JudgeEvent::TestStart {
            id: "1".to_string(),
            subtask: None,
            slot: None,
        };
        let s_none = serde_json::to_string(&ev_none).unwrap();
        assert!(
            !s_none.contains("\"slot\""),
            "Should omit None slot: {s_none}"
        );

        // 3. Serializing with slot: Some(2) should output "slot": 2
        let ev_some = JudgeEvent::TestStart {
            id: "1".to_string(),
            subtask: None,
            slot: Some(2),
        };
        let s_some = serde_json::to_string(&ev_some).unwrap();
        assert!(
            s_some.contains(r#""slot":2"#),
            "Should contain slot: {s_some}"
        );

        let de_some: JudgeEvent = serde_json::from_str(&s_some).unwrap();
        assert_eq!(ev_some, de_some);
    }

    #[test]
    fn test_pool_event_roundtrip() {
        let pool_ev = PoolEvent {
            submission: SubmissionRef {
                id: 42,
                problem: "aplusb".to_string(),
                name: "alice".to_string(),
            },
            inner: JudgeEvent::TestStart {
                id: "test_3".to_string(),
                subtask: Some(1),
                slot: Some(4),
            },
        };

        let json_str = serde_json::to_string(&pool_ev).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Must have submission, event, data top-level keys
        assert!(val.get("submission").is_some(), "submission key missing");
        assert!(val.get("event").is_some(), "event key missing");
        assert!(val.get("data").is_some(), "data key missing");

        assert_eq!(val["submission"]["id"], 42);
        assert_eq!(val["submission"]["problem"], "aplusb");
        assert_eq!(val["submission"]["name"], "alice");
        assert_eq!(val["event"], "test_start");
        assert_eq!(val["data"]["id"], "test_3");
        assert_eq!(val["data"]["slot"], 4);

        // Deserialization roundtrip
        let roundtrip: PoolEvent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(pool_ev, roundtrip);
    }
}

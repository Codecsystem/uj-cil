use crate::result::{JudgeEvent, PoolEvent, SubmissionJudgeReport};
use schemars::schema_for;
use uj_core::config::ProblemConfig;

pub fn generate_result_schema() -> String {
    let schema = schema_for!(SubmissionJudgeReport);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
}

pub fn generate_config_schema() -> String {
    let schema = schema_for!(ProblemConfig);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
}

#[derive(Debug, Clone, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AnyJudgeEvent {
    Single(JudgeEvent),
    Pool(PoolEvent),
}

pub fn generate_events_schema() -> String {
    let schema = schema_for!(AnyJudgeEvent);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
}

pub fn generate_pool_events_schema() -> String {
    let schema = schema_for!(PoolEvent);
    serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string())
}

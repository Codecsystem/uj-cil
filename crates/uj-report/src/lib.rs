//! uj-report: Structured report formats (JSON schema, NDJSON, HTML, CSV).

pub mod csv;
pub mod html;
pub mod human;
pub mod result;
pub mod schema;

pub use csv::{generate_csv, generate_multi_csv};
pub use html::{generate_html, generate_multi_html};
pub use human::{format_human_report, format_test_details};
pub use result::{
    DiffSummary, ExitInfo, JudgeEvent, PoolEvent, SingleTestResult, SubmissionJudgeReport,
    SubmissionRef, SubtaskResult, TimingInfo,
};

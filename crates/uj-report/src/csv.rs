//! CSV report generation for judge results.

use crate::result::SubmissionJudgeReport;

/// Escapes a CSV cell if it contains delimiters, quotes, or newlines, and neutralizes
/// spreadsheet formula injection (CWE-1236) by prefixing `'` when the cell starts with
/// `=`, `+`, `-`, `@`, `\t`, or `\r`.
fn escape_csv(val: &str) -> String {
    let safe = if val.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{val}")
    } else {
        val.to_string()
    };
    if safe.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", safe.replace('"', "\"\""))
    } else {
        safe
    }
}

/// Generates a CSV report for a single submission.
///
/// Header format: `submission,problem,test,verdict,time_ms,memory_kb,score` (prefixed with a UTF-8 BOM).
pub fn generate_csv(report: &SubmissionJudgeReport) -> String {
    generate_multi_csv(std::slice::from_ref(report))
}

/// Generates a CSV report for multiple submissions (e.g. contest or batch judging).
///
/// Prepends a UTF-8 BOM (`\u{FEFF}`) so Excel on Windows opens non-ASCII participant/problem names
/// without mojibake.
pub fn generate_multi_csv(reports: &[SubmissionJudgeReport]) -> String {
    let mut out = String::from("\u{FEFF}submission,problem,test,verdict,time_ms,memory_kb,score\n");

    for report in reports {
        let sub = escape_csv(&report.submission);
        let prob = escape_csv(&report.problem);

        for test in &report.tests {
            let test_id = escape_csv(&test.id);
            let verdict = escape_csv(test.verdict.short_code());
            let time_ms = test.time_ms;
            let memory_kb = test.memory_kb;
            let score = test.score;

            out.push_str(&format!(
                "{sub},{prob},{test_id},{verdict},{time_ms},{memory_kb},{score:.2}\n"
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uj_core::verdict::Verdict;

    #[test]
    fn test_generate_csv() {
        let report = SubmissionJudgeReport {
            schema_version: 1,
            problem: "aplusb".to_string(),
            submission: "ac.cpp".to_string(),
            verdict: Verdict::Accepted,
            score: 100.0,
            max_time_ms: 12,
            max_memory_kb: 1024,
            subtasks: vec![],
            tests: vec![
                crate::result::SingleTestResult {
                    id: "1".to_string(),
                    subtask: None,
                    verdict: Verdict::Accepted,
                    score: 50.0,
                    time_ms: 5,
                    wall_ms: 6,
                    memory_kb: 512,
                    exit: Default::default(),
                    message: None,
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: None,
                },
                crate::result::SingleTestResult {
                    id: "2".to_string(),
                    subtask: None,
                    verdict: Verdict::Accepted,
                    score: 50.0,
                    time_ms: 7,
                    wall_ms: 8,
                    memory_kb: 640,
                    exit: Default::default(),
                    message: None,
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: None,
                },
            ],
        };

        let csv = generate_csv(&report);
        assert!(csv.starts_with('\u{FEFF}'), "CSV must start with UTF-8 BOM");
        let lines: Vec<&str> = csv.trim_start_matches('\u{FEFF}').lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "submission,problem,test,verdict,time_ms,memory_kb,score"
        );
        assert_eq!(lines[1], "ac.cpp,aplusb,1,AC,5,512,50.00");
        assert_eq!(lines[2], "ac.cpp,aplusb,2,AC,7,640,50.00");
    }

    #[test]
    fn test_csv_formula_injection_guard() {
        for dangerous in [
            "=cmd|' /C calc'!A0",
            "+SUM(A1:A2)",
            "-2+3",
            "@IMPORTXML()",
            "\t=1",
            "\r=1",
        ] {
            let escaped = escape_csv(dangerous);
            let unquoted = escaped.trim_matches('"');
            assert!(
                unquoted.starts_with('\''),
                "Cell {dangerous:?} must be prefixed with single quote, got {escaped:?}"
            );
        }
        assert_eq!(escape_csv("=1,2\"3"), "\"'=1,2\"\"3\"");
    }
}

//! Human-readable text rendering for judge reports and test case inspection.

use crate::result::{SingleTestResult, SubmissionJudgeReport};

const MAX_HUMAN_SNIPPET_CHARS: usize = 200;
const MAX_HUMAN_CHECKER_CHARS: usize = 1024;

/// Sanitizes untrusted text before printing to a human terminal, escaping control characters
/// (such as `ESC` `\x1b`, `OSC`, `BEL`, `BS`, `CR`, and C1 controls) so contestant output cannot
/// inject terminal escape sequences, and bounding length to `max_chars`.
pub fn sanitize_terminal_text(s: &str, allow_newlines: bool, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars + 16));
    for (count, ch) in s.chars().enumerate() {
        if count >= max_chars {
            out.push_str("...");
            break;
        }
        if allow_newlines && ch == '\n' {
            out.push('\n');
        } else if ch.is_control() {
            for esc in ch.escape_default() {
                out.push(esc);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn format_human_report(report: &SubmissionJudgeReport) -> String {
    let mut out = String::new();
    let problem = sanitize_terminal_text(&report.problem, false, MAX_HUMAN_SNIPPET_CHARS);
    let submission = sanitize_terminal_text(&report.submission, false, MAX_HUMAN_SNIPPET_CHARS);
    out.push_str(&format!(
        "=== Judging Report: {problem} (Submission: {submission}) ===\n"
    ));
    out.push_str(&format!(
        "{:<6} {:<12} {:<6} {:<10} {:<12} {:<8} {}\n",
        "#", "Test ID", "Verd", "Time", "Memory", "Score", "Details"
    ));
    out.push_str(&format!("{}\n", "-".repeat(70)));

    for (i, t) in report.tests.iter().enumerate() {
        let mem_str = if t.memory_kb > 1024 {
            format!("{:.2} MiB", t.memory_kb as f64 / 1024.0)
        } else {
            format!("{} KiB", t.memory_kb)
        };
        let time_str = format!("{} ms", t.time_ms);
        let score_str = format!("{:.1}", t.score);
        let details = if let Some(ref d) = t.diff {
            let exp = sanitize_terminal_text(&d.expected, false, MAX_HUMAN_SNIPPET_CHARS);
            let got = sanitize_terminal_text(&d.found, false, MAX_HUMAN_SNIPPET_CHARS);
            format!(
                "line {}, col {}: exp '{}', got '{}'",
                d.line, d.column, exp, got
            )
        } else if let Some(ref msg) = t.checker {
            sanitize_terminal_text(msg, false, MAX_HUMAN_SNIPPET_CHARS)
        } else {
            String::new()
        };
        let test_id = sanitize_terminal_text(&t.id, false, MAX_HUMAN_SNIPPET_CHARS);

        out.push_str(&format!(
            "{:<6} {:<12} {:<6} {:<10} {:<12} {:<8} {}\n",
            i + 1,
            test_id,
            t.verdict.short_code(),
            time_str,
            mem_str,
            score_str,
            details
        ));
    }

    out.push_str(&format!("{}\n", "-".repeat(70)));
    if !report.subtasks.is_empty() {
        for st in &report.subtasks {
            out.push_str(&format!(
                "Subtask {}: [{}] {:.1} / {:.1} pts\n",
                st.id,
                st.verdict.short_code(),
                st.score,
                st.max_score
            ));
        }
        out.push_str(&format!("{}\n", "-".repeat(70)));
    }

    let peak_mem_str = if report.max_memory_kb > 1024 {
        format!("{:.2} MiB", report.max_memory_kb as f64 / 1024.0)
    } else {
        format!("{} KiB", report.max_memory_kb)
    };

    out.push_str(&format!(
        "Final Verdict: {} | Score: {:.1} | Max Time: {} ms | Peak Mem: {}\n",
        report.verdict.display_name(),
        report.score,
        report.max_time_ms,
        peak_mem_str
    ));

    out
}

pub fn format_test_details(test: &SingleTestResult) -> String {
    let mut out = String::new();
    let test_id = sanitize_terminal_text(&test.id, false, MAX_HUMAN_SNIPPET_CHARS);
    out.push_str(&format!("=== Test: {test_id} ===\n"));
    out.push_str(&format!(
        "Verdict:   {} ({})\n",
        test.verdict.display_name(),
        test.verdict.short_code()
    ));
    out.push_str(&format!("Score:     {:.1}\n", test.score));
    out.push_str(&format!("CPU Time:  {} ms\n", test.time_ms));
    out.push_str(&format!("Wall Time: {} ms\n", test.wall_ms));
    out.push_str(&format!("Memory:    {} KiB\n", test.memory_kb));
    if let Some(c) = test.exit.code {
        out.push_str(&format!("Exit Code: {}\n", c));
    }
    if let Some(nt) = test.exit.nt_status {
        out.push_str(&format!("NTSTATUS:  0x{:08X}\n", nt));
    }
    if let Some(ref msg) = test.message {
        let safe_msg = sanitize_terminal_text(msg, false, MAX_HUMAN_CHECKER_CHARS);
        out.push_str(&format!("Message:   {safe_msg}\n"));
    }
    if let Some(ref d) = test.diff {
        let exp = sanitize_terminal_text(&d.expected, false, MAX_HUMAN_SNIPPET_CHARS);
        let got = sanitize_terminal_text(&d.found, false, MAX_HUMAN_SNIPPET_CHARS);
        out.push_str("\n--- Diff Summary ---\n");
        out.push_str(&format!(
            "Line: {}\nColumn: {}\nToken: {}\n",
            d.line, d.column, d.token_index
        ));
        out.push_str(&format!("Expected: {exp}\n"));
        out.push_str(&format!("Found:    {got}\n"));
    }
    if let Some(ref chk) = test.checker {
        let safe_chk = sanitize_terminal_text(chk, true, MAX_HUMAN_CHECKER_CHARS);
        out.push_str("\n--- Checker Output ---\n");
        out.push_str(&safe_chk);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::DiffSummary;
    use uj_core::verdict::Verdict;

    #[test]
    fn test_human_output_escapes_terminal_control_sequences() {
        let malicious = "\x1b[2J\x1b]52;c;YWJj\x07evil\r\noverwrite";
        let test = SingleTestResult {
            id: "1".to_string(),
            subtask: None,
            verdict: Verdict::WrongAnswer,
            score: 0.0,
            time_ms: 5,
            wall_ms: 6,
            memory_kb: 512,
            exit: Default::default(),
            message: Some(malicious.to_string()),
            checker: Some(malicious.to_string()),
            diff: Some(DiffSummary {
                line: 1,
                column: 1,
                token_index: 1,
                expected: "42".to_string(),
                found: malicious.to_string(),
                truncated: false,
            }),
            timing: Default::default(),
            sandbox_tier: None,
        };
        let report = SubmissionJudgeReport {
            schema_version: 1,
            problem: "prob".to_string(),
            submission: "sub.cpp".to_string(),
            verdict: Verdict::WrongAnswer,
            score: 0.0,
            max_time_ms: 5,
            max_memory_kb: 512,
            subtasks: vec![],
            tests: vec![test.clone()],
        };

        let human = format_human_report(&report);
        let details = format_test_details(&test);

        assert!(
            !human.contains('\x1b') && !human.contains('\x07') && !human.contains('\r'),
            "Human report must not contain raw ESC, BEL, or CR control characters"
        );
        assert!(
            !details.contains('\x1b') && !details.contains('\x07') && !details.contains('\r'),
            "Test details must not contain raw ESC, BEL, or CR control characters"
        );
        assert!(human.contains("\\u{1b}"));
        assert!(details.contains("\\u{1b}"));
    }
}

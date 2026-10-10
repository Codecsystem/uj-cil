//! Human-readable text rendering for judge reports and test case inspection.

use crate::result::{SingleTestResult, SubmissionJudgeReport};

pub fn format_human_report(report: &SubmissionJudgeReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "=== Judging Report: {} (Submission: {}) ===\n",
        report.problem, report.submission
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
            format!(
                "line {}, col {}: exp '{}', got '{}'",
                d.line, d.column, d.expected, d.found
            )
        } else if let Some(ref msg) = t.checker {
            msg.clone()
        } else {
            String::new()
        };

        out.push_str(&format!(
            "{:<6} {:<12} {:<6} {:<10} {:<12} {:<8} {}\n",
            i + 1,
            t.id,
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
    out.push_str(&format!("=== Test: {} ===\n", test.id));
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
    if let Some(ref d) = test.diff {
        out.push_str("\n--- Diff Summary ---\n");
        out.push_str(&format!(
            "Line: {}\nColumn: {}\nToken: {}\n",
            d.line, d.column, d.token_index
        ));
        out.push_str(&format!("Expected: {}\n", d.expected));
        out.push_str(&format!("Found:    {}\n", d.found));
    }
    if let Some(ref chk) = test.checker {
        out.push_str("\n--- Checker Output ---\n");
        out.push_str(chk);
        out.push('\n');
    }
    out
}

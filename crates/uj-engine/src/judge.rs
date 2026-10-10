//! Test case execution, verdict evaluation, and subtask aggregation.

use std::path::Path;
use std::time::Instant;

use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{ProblemConfig, ScoringType};
use uj_core::run::{ExitStatus, KillReason, Limits, RunSpec, Stdio, Tier};
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{
    DiffSummary, ExitInfo, SingleTestResult, SubmissionJudgeReport, SubtaskResult, TimingInfo,
};
use uj_sandbox::{CancelToken, Sandbox};

#[derive(Debug, Clone)]
pub struct JudgeOptions {
    pub sandbox_mode: String,
    pub require_tier: bool,
    pub fail_fast: bool,
    pub precise: bool,
}

impl Default for JudgeOptions {
    fn default() -> Self {
        Self {
            sandbox_mode: "auto".to_string(),
            require_tier: false,
            fail_fast: false,
            precise: true,
        }
    }
}

/// Execute a single test case within the sandbox and evaluate verdict.
#[allow(clippy::too_many_arguments)]
pub fn judge_single_test(
    sandbox: &dyn Sandbox,
    exe_path: &Path,
    interpreter_cmd: Option<&[String]>,
    test: &TestCase,
    limits: &Limits,
    checker_type: BuiltinCheckerType,
    pe_as_wa: bool,
    cpu_affinity: Option<usize>,
    cancel: &CancelToken,
) -> SingleTestResult {
    let argv = if let Some(cmd) = interpreter_cmd {
        cmd.iter().map(std::ffi::OsString::from).collect()
    } else {
        vec![exe_path.as_os_str().to_os_string()]
    };

    let work_dir = std::env::temp_dir();

    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::File(test.input_path.clone()),
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: limits.clone(),
        cpu_affinity: cpu_affinity.map(|c| vec![c]),
        seccomp_profile: Default::default(),
    };

    let start_instant = Instant::now();
    let run_res = sandbox.run(&spec, cancel);
    let duration_ms = start_instant.elapsed().as_millis() as u64;

    let res = match run_res {
        Ok(r) => r,
        Err(e) => {
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: duration_ms,
                memory_kb: 0,
                exit: ExitInfo::default(),
                checker: Some(format!("Sandbox error: {e}")),
                diff: None,
                timing: TimingInfo {
                    rejudged: false,
                    confidence: "low".to_string(),
                    samples_ms: vec![],
                },
            };
        }
    };

    let time_ms = (res.cpu_time_us / 1000).max(1);
    let wall_ms = (res.wall_time_us / 1000).max(1);
    let memory_kb = res.memory_peak_bytes / 1024;

    let exit_info = match res.status {
        ExitStatus::Exited(c) => ExitInfo {
            code: Some(c),
            signal: None,
            nt_status: None,
        },
        ExitStatus::Signaled(s) => ExitInfo {
            code: None,
            signal: Some(s),
            nt_status: None,
        },
        ExitStatus::NtStatus(nt) => ExitInfo {
            code: None,
            signal: None,
            nt_status: Some(nt),
        },
    };

    // ------------------------------------------------------------------------
    // Section 5.4 Verdict Order Evaluation
    // ------------------------------------------------------------------------
    let (mut verdict, score_ratio, checker_msg, diff_summary) = match res.killed_by {
        Some(KillReason::CpuLimit) => (Verdict::TimeLimitExceeded, 0.0, None, None),
        Some(KillReason::WallLimit) => (
            Verdict::TimeLimitExceeded,
            0.0,
            if time_ms < limits.cpu_time_ms / 2 {
                Some("Process idleness detected (waiting on input or sleeping)".to_string())
            } else {
                None
            },
            None,
        ),
        Some(KillReason::MemoryLimit) => (Verdict::MemoryLimitExceeded, 0.0, None, None),
        Some(KillReason::OutputLimit) => (Verdict::OutputLimitExceeded, 0.0, None, None),
        Some(KillReason::ProcessLimit) => (
            Verdict::RuntimeError,
            0.0,
            Some("Process limit exceeded".to_string()),
            None,
        ),
        Some(KillReason::Seccomp) => (
            Verdict::RestrictedFunction,
            0.0,
            Some("Restricted syscall blocked by security policy".to_string()),
            None,
        ),
        Some(KillReason::UserCancel) => (
            Verdict::JudgeFailed,
            0.0,
            Some("Cancelled by user".to_string()),
            None,
        ),
        None => {
            // Check limits even if process exited cleanly
            if time_ms > limits.cpu_time_ms {
                (Verdict::TimeLimitExceeded, 0.0, None, None)
            } else if res.oom_killed || res.memory_peak_bytes > limits.memory_bytes {
                (Verdict::MemoryLimitExceeded, 0.0, None, None)
            } else if !res.status.is_success() {
                let msg = match res.status {
                    ExitStatus::Exited(c) => format!("Process exited with return code {c}"),
                    ExitStatus::Signaled(s) => format!("Terminated by signal {s}"),
                    ExitStatus::NtStatus(nt) => {
                        let name = uj_sandbox::ntstatus::format_ntstatus(nt);
                        format!("Process crashed: {name} (0x{nt:08X})")
                    }
                };
                (Verdict::RuntimeError, 0.0, Some(msg), None)
            } else {
                // Program exited cleanly (Exit code 0), run checker
                let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
                let ans_bytes = test
                    .answer_path
                    .as_ref()
                    .and_then(|p| uj_checker::read_file_bytes(p).ok())
                    .unwrap_or_default();

                let check_res = checker_type.check(&stdout_bytes, &ans_bytes);
                let diff = check_res.diff.map(|d| DiffSummary {
                    line: d.line,
                    column: d.column,
                    token_index: d.token_index,
                    expected: d.expected,
                    found: d.actual,
                    truncated: false,
                });

                (
                    check_res.verdict,
                    check_res.score_ratio,
                    check_res.message,
                    diff,
                )
            }
        }
    };

    if pe_as_wa && verdict == Verdict::PresentationError {
        verdict = Verdict::WrongAnswer;
    }

    let allocated_score = (test.weight as f64 / 1_000_000.0) * score_ratio;

    SingleTestResult {
        id: test.name.clone(),
        subtask: test.subtask_id,
        verdict,
        score: allocated_score,
        time_ms,
        wall_ms,
        memory_kb,
        exit: exit_info,
        checker: checker_msg,
        diff: diff_summary,
        timing: TimingInfo {
            rejudged: false,
            confidence: if res.sandbox_tier == Tier::Full {
                "high".to_string()
            } else {
                "medium".to_string()
            },
            samples_ms: vec![time_ms],
        },
    }
}

/// Aggregate single test results into subtasks and overall submission report.
pub fn aggregate_submission_report(
    problem_name: &str,
    submission_name: &str,
    config: &ProblemConfig,
    test_results: Vec<SingleTestResult>,
) -> SubmissionJudgeReport {
    let mut subtask_map: std::collections::BTreeMap<u32, Vec<&SingleTestResult>> =
        std::collections::BTreeMap::new();
    let mut unassigned_tests: Vec<&SingleTestResult> = Vec::new();

    for tr in &test_results {
        if let Some(st_id) = tr.subtask {
            subtask_map.entry(st_id).or_default().push(tr);
        } else {
            unassigned_tests.push(tr);
        }
    }

    let mut subtask_results = Vec::new();
    let mut total_score = 0.0;
    let mut worst_verdict = Verdict::Accepted;

    // Evaluate subtasks defined in config
    for st_cfg in &config.subtasks {
        let tests = subtask_map.remove(&st_cfg.id).unwrap_or_default();
        let test_ids: Vec<String> = tests.iter().map(|t| t.id.clone()).collect();

        // Check dependencies
        let mut dep_failed = false;
        for &dep_id in &st_cfg.depends {
            if let Some(dep_res) = subtask_results
                .iter()
                .find(|s: &&SubtaskResult| s.id == dep_id)
                && dep_res.score < dep_res.max_score
            {
                dep_failed = true;
                break;
            }
        }

        let (st_verdict, st_score) = if dep_failed {
            (Verdict::Skipped, 0.0)
        } else if tests.is_empty() {
            (Verdict::Accepted, st_cfg.score)
        } else {
            let mut worst = Verdict::Accepted;
            let mut min_ratio = 1.0f64;
            let mut ratio_sum = 0.0f64;

            for t in &tests {
                worst = worst.merge(t.verdict);
                let ratio = if t.score > 0.0 && t.verdict.is_accepted() {
                    1.0
                } else {
                    0.0
                };
                if ratio < min_ratio {
                    min_ratio = ratio;
                }
                ratio_sum += ratio;
            }

            let final_ratio = match st_cfg.scoring {
                ScoringType::Min => min_ratio,
                ScoringType::Sum => ratio_sum / tests.len() as f64,
            };

            (worst, st_cfg.score * final_ratio)
        };

        worst_verdict = worst_verdict.merge(st_verdict);
        total_score += st_score;

        subtask_results.push(SubtaskResult {
            id: st_cfg.id,
            score: st_score,
            max_score: st_cfg.score,
            verdict: st_verdict,
            test_ids,
        });
    }

    // Handle any unassigned tests
    if !unassigned_tests.is_empty() {
        for t in &unassigned_tests {
            worst_verdict = worst_verdict.merge(t.verdict);
            total_score += t.score;
        }
    }

    let max_time_ms = test_results.iter().map(|t| t.time_ms).max().unwrap_or(0);
    let max_memory_kb = test_results.iter().map(|t| t.memory_kb).max().unwrap_or(0);

    SubmissionJudgeReport {
        schema_version: 1,
        problem: problem_name.to_string(),
        submission: submission_name.to_string(),
        verdict: worst_verdict,
        score: total_score,
        max_time_ms,
        max_memory_kb,
        subtasks: subtask_results,
        tests: test_results,
    }
}

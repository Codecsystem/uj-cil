use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{ProblemConfig, ScoringType};
use uj_core::run::{ExitStatus, KillReason, Limits, RunSpec, Stdio, Tier};
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{
    DiffSummary, ExitInfo, SingleTestResult, SubmissionJudgeReport, SubtaskResult, TimingInfo,
};
use uj_sandbox::{CancelToken, PipeConnection, Sandbox, Wiring};

#[derive(Debug, Clone)]
pub enum ResolvedChecker {
    Builtin(BuiltinCheckerType),
    Testlib(PathBuf),
    Lemon { path: PathBuf, full_score: f64 },
}

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
    checker: &ResolvedChecker,
    pe_as_wa: bool,
    cpu_affinity: Option<usize>,
    io: Option<&uj_core::config::IoConfig>,
    cancel: &CancelToken,
) -> SingleTestResult {
    let base_dir = test
        .input_path
        .parent()
        .and_then(|p| {
            if p.file_name().is_some_and(|n| n == "tests") {
                p.parent()
            } else {
                Some(p)
            }
        })
        .unwrap_or_else(|| Path::new("."));

    let box_dir = sandbox
        .create_box_dir(base_dir)
        .unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir());
    let run_dir = box_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| box_dir.clone());

    let is_file_io = io.is_some_and(|i| {
        i.mode == uj_core::config::IoMode::File || i.input_name.is_some() || i.output_name.is_some()
    });
    let in_name = io.and_then(|i| i.input_name.as_deref()).unwrap_or("input.txt");
    let out_name = io.and_then(|i| i.output_name.as_deref()).unwrap_or("output.txt");

    if is_file_io {
        let _ = fs::copy(&test.input_path, box_dir.join(in_name));
    }

    let argv = if let Some(cmd) = interpreter_cmd {
        let mut modified_cmd = Vec::new();
        for (i, part) in cmd.iter().enumerate() {
            if i == 0 {
                modified_cmd.push(std::ffi::OsString::from(part));
            } else if Path::new(part).exists() {
                let script_name = Path::new(part)
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("prog.py"));
                let box_script = box_dir.join(script_name);
                let _ = std::fs::copy(part, &box_script);
                modified_cmd.push(box_script.into_os_string());
            } else {
                modified_cmd.push(std::ffi::OsString::from(part));
            }
        }
        modified_cmd
    } else {
        let authorized_exe = uj_sandbox::get_or_prepare_authorized_binary(exe_path, base_dir)
            .unwrap_or_else(|_| exe_path.to_path_buf());
        vec![authorized_exe.into_os_string()]
    };

    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir: box_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::File(test.input_path.clone()),
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: limits.clone(),
        cpu_affinity: cpu_affinity.map(|c| vec![c]),
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let start_instant = Instant::now();
    #[allow(unused_mut)]
    let mut run_res = sandbox.run(&spec, cancel);
    #[allow(unused_mut)]
    let mut duration_ms = start_instant.elapsed().as_millis() as u64;

    // Windows Defender mitigation retry:
    // Only allowed on the FIRST execution of a binary to avoid 10s stalls on real blocking/sleep tests.
    #[cfg(windows)]
    static RETRIED_BINARIES: std::sync::LazyLock<
        parking_lot::Mutex<std::collections::HashSet<PathBuf>>,
    > = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

    #[cfg(windows)]
    if let Ok(ref res) = run_res {
        let cpu_ms = res.cpu_time_us / 1000;
        let threshold_ms = ((limits.cpu_time_ms / 10).max(50)).min(limits.cpu_time_ms);
        if res.killed_by == Some(KillReason::WallLimit)
            && cpu_ms < threshold_ms
            && !cancel.is_cancelled()
        {
            let should_retry = {
                let mut set = RETRIED_BINARIES.lock();
                set.insert(exe_path.to_path_buf())
            };
            if should_retry {
                let mut retry_spec = spec.clone();
                retry_spec.limits.wall_time_ms = retry_spec.limits.wall_time_ms.max(10000);
                let retry_start = Instant::now();
                if let Ok(retry_res) = sandbox.run(&retry_spec, cancel) {
                    run_res = Ok(retry_res);
                    duration_ms = retry_start.elapsed().as_millis() as u64;
                }
            }
        }
    }

    let res = match run_res {
        Ok(r) => r,
        Err(e) => {
            let _ = fs::remove_dir_all(&run_dir);
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: duration_ms,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some(format!("Sandbox error: {e}")),
                checker: None,
                diff: None,
                timing: TimingInfo {
                    rejudged: false,
                    confidence: "low".to_string(),
                    samples_ms: vec![],
                },
                sandbox_tier: Some(sandbox.tier()),
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
    // MLE takes precedence over RuntimeError (e.g. bad_alloc / fastfail crash)
    // ------------------------------------------------------------------------
    let (mut verdict, score_ratio, test_message, checker_msg, diff_summary) = match res.killed_by {
        Some(KillReason::UserCancel) => (
            Verdict::JudgeFailed,
            0.0,
            Some("Cancelled by user".to_string()),
            None,
            None,
        ),
        Some(KillReason::MemoryLimit) => {
            if res.status == ExitStatus::NtStatus(0xC00000FD) {
                let name = uj_sandbox::ntstatus::format_ntstatus(0xC00000FD);
                (
                    Verdict::RuntimeError,
                    0.0,
                    Some(format!("Process crashed: {name} (0xC00000FD)")),
                    None,
                    None,
                )
            } else {
                (Verdict::MemoryLimitExceeded, 0.0, None, None, None)
            }
        }
        Some(KillReason::OutputLimit) => (Verdict::OutputLimitExceeded, 0.0, None, None, None),
        Some(KillReason::CpuLimit) => (Verdict::TimeLimitExceeded, 0.0, None, None, None),
        Some(KillReason::WallLimit) => (
            Verdict::TimeLimitExceeded,
            0.0,
            if time_ms < limits.cpu_time_ms / 2 {
                Some("Process idleness detected (waiting on input or sleeping)".to_string())
            } else {
                None
            },
            None,
            None,
        ),
        Some(KillReason::Seccomp) => (
            Verdict::RestrictedFunction,
            0.0,
            Some("Restricted syscall blocked by security policy".to_string()),
            None,
            None,
        ),
        Some(KillReason::ProcessLimit) => (
            Verdict::RuntimeError,
            0.0,
            Some("Process limit exceeded".to_string()),
            None,
            None,
        ),
        None => {
            // Check limits even if process exited cleanly
            if res.status == ExitStatus::NtStatus(0xC00000FD) {
                let name = uj_sandbox::ntstatus::format_ntstatus(0xC00000FD);
                (
                    Verdict::RuntimeError,
                    0.0,
                    Some(format!("Process crashed: {name} (0xC00000FD)")),
                    None,
                    None,
                )
            } else if res.oom_killed || res.memory_peak_bytes > limits.memory_bytes {
                (Verdict::MemoryLimitExceeded, 0.0, None, None, None)
            } else if time_ms > limits.cpu_time_ms {
                (Verdict::TimeLimitExceeded, 0.0, None, None, None)
            } else if !res.status.is_success() {
                let msg = match res.status {
                    ExitStatus::Exited(c) => format!("Process exited with return code {c}"),
                    ExitStatus::Signaled(s) => format!("Terminated by signal {s}"),
                    ExitStatus::NtStatus(nt) => {
                        let name = uj_sandbox::ntstatus::format_ntstatus(nt);
                        format!("Process crashed: {name} (0x{nt:08X})")
                    }
                };
                (Verdict::RuntimeError, 0.0, Some(msg), None, None)
            } else {
                // Program exited cleanly (Exit code 0), run checker
                let stdout_bytes = if is_file_io && box_dir.join(out_name).exists() {
                    fs::read(box_dir.join(out_name)).unwrap_or_default()
                } else {
                    res.stdout.map(|o| o.bytes).unwrap_or_default()
                };
                match checker {
                    ResolvedChecker::Builtin(b) => {
                        let ans_bytes = test
                            .answer_path
                            .as_ref()
                            .and_then(|p| uj_checker::read_file_bytes(p).ok())
                            .unwrap_or_default();

                        let check_res = b.check(&stdout_bytes, &ans_bytes);
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
                            None,
                            check_res.message,
                            diff,
                        )
                    }
                    ResolvedChecker::Testlib(chk_exe) => {
                        let out_file = run_dir.join(format!(
                            "uj_out_{}_{}_{}.txt",
                            std::process::id(),
                            test.name,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        ));
                        let _ = fs::write(&out_file, &stdout_bytes);
                        let empty_path = PathBuf::new();
                        let ans_file = test.answer_path.as_ref().unwrap_or(&empty_path);

                        let check_res = uj_checker::testlib::run_testlib_checker(
                            sandbox,
                            chk_exe,
                            &test.input_path,
                            &out_file,
                            ans_file,
                            pe_as_wa,
                            cancel,
                        );
                        let _ = fs::remove_file(&out_file);
                        (
                            check_res.verdict,
                            check_res.score_ratio,
                            None,
                            check_res.message,
                            None,
                        )
                    }
                    ResolvedChecker::Lemon {
                        path: chk_exe,
                        full_score,
                    } => {
                        let out_file = run_dir.join(format!(
                            "uj_out_{}_{}_{}.txt",
                            std::process::id(),
                            test.name,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        ));
                        let _ = fs::write(&out_file, &stdout_bytes);
                        let empty_path = PathBuf::new();
                        let ans_file = test.answer_path.as_ref().unwrap_or(&empty_path);

                        let check_res = uj_checker::lemon::run_lemon_checker(
                            sandbox,
                            chk_exe,
                            &test.input_path,
                            &out_file,
                            ans_file,
                            *full_score,
                            cancel,
                        );
                        let _ = fs::remove_file(&out_file);
                        (
                            check_res.verdict,
                            check_res.score_ratio,
                            None,
                            check_res.message,
                            None,
                        )
                    }
                }
            }
        }
    };

    if pe_as_wa && verdict == Verdict::PresentationError {
        verdict = Verdict::WrongAnswer;
    }

    let allocated_score = (test.weight as f64 / 1_000_000.0) * score_ratio;

    // Clean up isolated run directory and all files inside it
    let _ = fs::remove_dir_all(&run_dir);

    SingleTestResult {
        id: test.name.clone(),
        subtask: test.subtask_id,
        verdict,
        score: allocated_score,
        time_ms,
        wall_ms,
        memory_kb,
        exit: exit_info,
        message: test_message,
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
        sandbox_tier: Some(res.sandbox_tier),
    }
}

/// Aggregate single test results into subtasks and overall submission report.
pub fn aggregate_submission_report(
    problem_name: &str,
    submission_name: &str,
    config: &ProblemConfig,
    test_results: Vec<SingleTestResult>,
) -> SubmissionJudgeReport {
    if test_results.is_empty() {
        return SubmissionJudgeReport {
            schema_version: 1,
            problem: problem_name.to_string(),
            submission: submission_name.to_string(),
            verdict: Verdict::JudgeFailed,
            score: 0.0,
            max_time_ms: 0,
            max_memory_kb: 0,
            subtasks: Vec::new(),
            tests: Vec::new(),
        };
    }

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
    let mut total_score_fixed: i64 = 0;
    let mut worst_verdict = Verdict::Accepted;

    // Evaluate subtasks defined in config using fixed-point integer math (scale 10_000)
    for st_cfg in &config.subtasks {
        let tests = subtask_map.remove(&st_cfg.id).unwrap_or_default();
        let test_ids: Vec<String> = tests.iter().map(|t| t.id.clone()).collect();
        let st_max_fixed = (st_cfg.score * 10000.0).round() as i64;

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

        let (st_verdict, st_score_fixed) = if dep_failed {
            (Verdict::Skipped, 0i64)
        } else if tests.is_empty() {
            (Verdict::JudgeFailed, 0i64)
        } else {
            let mut worst = Verdict::Accepted;
            for t in &tests {
                worst = worst.merge(t.verdict);
            }

            let score_fixed = match st_cfg.scoring {
                ScoringType::Min => {
                    let min_ratio = tests
                        .iter()
                        .map(|t| {
                            if st_cfg.score > 0.0 {
                                (t.score / st_cfg.score).clamp(0.0, 1.0)
                            } else {
                                0.0
                            }
                        })
                        .fold(1.0f64, f64::min);
                    ((st_max_fixed as f64) * min_ratio).round() as i64
                }
                ScoringType::Sum => tests
                    .iter()
                    .map(|t| (t.score * 10000.0).round() as i64)
                    .sum(),
            };

            let final_verdict = if score_fixed >= st_max_fixed {
                worst
            } else if score_fixed > 0 {
                Verdict::PartiallyCorrect
            } else {
                worst
            };

            (final_verdict, score_fixed)
        };

        let st_score = st_score_fixed as f64 / 10000.0;
        worst_verdict = worst_verdict.merge(st_verdict);
        total_score_fixed += st_score_fixed;

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
            total_score_fixed += (t.score * 10000.0).round() as i64;
        }
    }

    let total_score = total_score_fixed as f64 / 10000.0;

    let max_possible_score: f64 = if !config.subtasks.is_empty() {
        config.subtasks.iter().map(|s| s.score).sum()
    } else {
        100.0
    };

    let final_submission_verdict = if test_results.is_empty() {
        Verdict::JudgeFailed
    } else if (total_score - max_possible_score).abs() < 1e-6 && max_possible_score > 0.0 {
        worst_verdict
    } else if total_score > 0.0 && worst_verdict == Verdict::WrongAnswer {
        Verdict::PartiallyCorrect
    } else {
        worst_verdict
    };

    let max_time_ms = test_results.iter().map(|t| t.time_ms).max().unwrap_or(0);
    let max_memory_kb = test_results.iter().map(|t| t.memory_kb).max().unwrap_or(0);

    SubmissionJudgeReport {
        schema_version: 1,
        problem: problem_name.to_string(),
        submission: submission_name.to_string(),
        verdict: final_submission_verdict,
        score: total_score,
        max_time_ms,
        max_memory_kb,
        subtasks: subtask_results,
        tests: test_results,
    }
}

/// Execute an interactive test case with solution and interactor wired together.
#[allow(clippy::too_many_arguments)]
pub fn judge_interactive_test(
    sandbox: &dyn Sandbox,
    exe_path: &Path,
    interpreter_cmd: Option<&[String]>,
    interactor_exe: &Path,
    test: &TestCase,
    limits: &Limits,
    checker: Option<&ResolvedChecker>,
    pe_as_wa: bool,
    cancel: &CancelToken,
) -> SingleTestResult {
    let base_dir = test
        .input_path
        .parent()
        .and_then(|p| {
            if p.file_name().is_some_and(|n| n == "tests") {
                p.parent()
            } else {
                Some(p)
            }
        })
        .unwrap_or_else(|| Path::new("."));

    let box_dir = sandbox
        .create_box_dir(base_dir)
        .unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir());
    let run_dir = box_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| box_dir.clone());

    let argv_sol = if let Some(cmd) = interpreter_cmd {
        let mut modified_cmd = Vec::new();
        for (i, part) in cmd.iter().enumerate() {
            if i == 0 {
                modified_cmd.push(std::ffi::OsString::from(part));
            } else if Path::new(part).exists() {
                let script_name = Path::new(part)
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("prog.py"));
                let box_script = box_dir.join(script_name);
                let _ = std::fs::copy(part, &box_script);
                modified_cmd.push(box_script.into_os_string());
            } else {
                modified_cmd.push(std::ffi::OsString::from(part));
            }
        }
        modified_cmd
    } else {
        let authorized_exe = uj_sandbox::get_or_prepare_authorized_binary(exe_path, base_dir)
            .unwrap_or_else(|_| exe_path.to_path_buf());
        vec![authorized_exe.into_os_string()]
    };

    let pid = std::process::id();
    let time_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let tout_file = run_dir.join(format!("interact_tout_{pid}_{time_now}.txt"));
    let result_file = run_dir.join(format!("interact_res_{pid}_{time_now}.xml"));

    let spec_sol = RunSpec {
        argv: argv_sol,
        env: vec![],
        work_dir: box_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Pipe,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: limits.clone(),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let mut argv_int = vec![
        interactor_exe.as_os_str().to_os_string(),
        test.input_path.as_os_str().to_os_string(),
        tout_file.as_os_str().to_os_string(),
    ];
    if let Some(ref ans_path) = test.answer_path {
        argv_int.push(ans_path.as_os_str().to_os_string());
    } else {
        argv_int.push(test.input_path.as_os_str().to_os_string());
    }
    argv_int.push(result_file.as_os_str().to_os_string());
    argv_int.push(std::ffi::OsString::from("-appes"));

    let mut int_limits = Limits::new(limits.cpu_time_ms * 2 + 5000, 1024 * 1024 * 1024);
    int_limits.wall_time_ms = limits.wall_time_ms + 2000;

    let spec_int = RunSpec {
        argv: argv_int,
        env: vec![],
        work_dir: run_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Pipe,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: int_limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let wiring = Wiring {
        connections: vec![
            PipeConnection {
                from_proc: 1, // interactor stdout -> solution stdin
                to_proc: 0,
            },
            PipeConnection {
                from_proc: 0, // solution stdout -> interactor stdin
                to_proc: 1,
            },
        ],
    };

    let start_instant = Instant::now();
    let group_res = sandbox.run_group(&[spec_sol, spec_int], &wiring, cancel);
    let duration_ms = start_instant.elapsed().as_millis() as u64;

    let results = match group_res {
        Ok(r) if r.len() >= 2 => r,
        Ok(_) => {
            let _ = fs::remove_dir_all(&run_dir);
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: duration_ms,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some("Incomplete process group results".to_string()),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(sandbox.tier()),
            };
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&run_dir);
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: duration_ms,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some(format!("Sandbox run_group error: {e}")),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(sandbox.tier()),
            };
        }
    };

    let sol_res = &results[0];
    let int_res = &results[1];

    let time_ms = (sol_res.cpu_time_us / 1000).max(1);
    let wall_ms = (sol_res.wall_time_us / 1000).max(1);
    let memory_kb = sol_res.memory_peak_bytes / 1024;

    let int_stderr = String::from_utf8_lossy(&int_res.stderr_tail).to_string();

    // Parse interactor outcome from -appes XML or exit code
    let interactor_check = if result_file.is_file() {
        if let Ok(xml) = fs::read_to_string(&result_file) {
            uj_checker::testlib::parse_appes_xml(&xml, pe_as_wa)
        } else {
            None
        }
    } else {
        None
    };

    let interactor_res = interactor_check.unwrap_or_else(|| {
        let code = match int_res.status {
            ExitStatus::Exited(c) => c,
            _ => 3, // FAIL
        };
        uj_checker::testlib::parse_testlib_exit_code(code, &int_stderr, pe_as_wa)
    });

    let _ = fs::remove_file(&result_file);

    let interactor_finished_first = match (int_res.exit_order, sol_res.exit_order) {
        (Some(i_ord), Some(s_ord)) => i_ord < s_ord,
        (Some(_), None) => true,
        _ => false,
    };

    // ------------------------------------------------------------------------
    // Section 7.4 Interactive Verdict Rules
    // ------------------------------------------------------------------------
    let (verdict, score_ratio, checker_msg) = if cancel.is_cancelled() {
        (
            Verdict::JudgeFailed,
            0.0,
            Some("Cancelled by user".to_string()),
        )
    } else if sol_res.killed_by == Some(KillReason::WallLimit)
        && (int_res.killed_by == Some(KillReason::WallLimit) || time_ms < limits.cpu_time_ms / 2)
    {
        // Rule 4: Both processes blocked or wall limit reached with low CPU -> Idleness limit exceeded
        (
            Verdict::TimeLimitExceeded,
            0.0,
            Some("Idleness limit exceeded (deadlock or blocked waiting on input)".to_string()),
        )
    } else if int_res.status.code() == Some(3)
        || interactor_res.verdict == Verdict::JudgeFailed
        || (!int_res.status.is_success()
            && int_res.status.code() != Some(1)
            && int_res.status.code() != Some(2))
    {
        // Rule 1: Interactor returns FAIL or crashes -> JudgeFailed
        (
            Verdict::JudgeFailed,
            0.0,
            interactor_res
                .message
                .or_else(|| Some("Interactor failed or crashed".to_string())),
        )
    } else if !interactor_finished_first {
        // Rule 2: Solution exceeded limits or crashed BEFORE interactor ended
        if sol_res.killed_by == Some(KillReason::CpuLimit) || time_ms > limits.cpu_time_ms {
            (Verdict::TimeLimitExceeded, 0.0, None)
        } else if sol_res.killed_by == Some(KillReason::MemoryLimit)
            || sol_res.oom_killed
            || sol_res.memory_peak_bytes > limits.memory_bytes
        {
            (Verdict::MemoryLimitExceeded, 0.0, None)
        } else if sol_res.killed_by == Some(KillReason::OutputLimit) {
            (Verdict::OutputLimitExceeded, 0.0, None)
        } else if sol_res.killed_by == Some(KillReason::Seccomp) {
            (
                Verdict::RestrictedFunction,
                0.0,
                Some("Restricted syscall blocked by security policy".to_string()),
            )
        } else if !sol_res.status.is_success() {
            let msg = match sol_res.status {
                ExitStatus::Exited(c) => format!("Process exited with return code {c}"),
                ExitStatus::Signaled(s) => format!("Terminated by signal {s}"),
                ExitStatus::NtStatus(nt) => {
                    let name = uj_sandbox::ntstatus::format_ntstatus(nt);
                    format!("Process crashed: {name} (0x{nt:08X})")
                }
            };
            (Verdict::RuntimeError, 0.0, Some(msg))
        } else {
            // Solution exited cleanly first, use interactor's verdict
            (
                interactor_res.verdict,
                interactor_res.score_ratio,
                interactor_res.message,
            )
        }
    } else {
        // Rule 3 & 5: Interactor finished first
        if interactor_res.verdict != Verdict::Accepted {
            // Interactor decided WA/PE/PC, solution exited afterward due to EOF/broken pipe -> not RE
            (
                interactor_res.verdict,
                interactor_res.score_ratio,
                interactor_res.message,
            )
        } else {
            // Interactor said Accepted!
            if let Some(chk) = checker
                && tout_file.is_file()
            {
                // Run post-checker on tout_file if configured
                let empty_path = PathBuf::new();
                let ans_file = test.answer_path.as_ref().unwrap_or(&empty_path);
                match chk {
                    ResolvedChecker::Builtin(b) => {
                        let tout_bytes = fs::read(&tout_file).unwrap_or_default();
                        let ans_bytes = uj_checker::read_file_bytes(ans_file).unwrap_or_default();
                        let check_res = b.check(&tout_bytes, &ans_bytes);
                        (check_res.verdict, check_res.score_ratio, check_res.message)
                    }
                    ResolvedChecker::Testlib(chk_exe) => {
                        let check_res = uj_checker::testlib::run_testlib_checker(
                            sandbox,
                            chk_exe,
                            &test.input_path,
                            &tout_file,
                            ans_file,
                            pe_as_wa,
                            cancel,
                        );
                        (check_res.verdict, check_res.score_ratio, check_res.message)
                    }
                    ResolvedChecker::Lemon {
                        path: chk_exe,
                        full_score,
                    } => {
                        let check_res = uj_checker::lemon::run_lemon_checker(
                            sandbox,
                            chk_exe,
                            &test.input_path,
                            &tout_file,
                            ans_file,
                            *full_score,
                            cancel,
                        );
                        (check_res.verdict, check_res.score_ratio, check_res.message)
                    }
                }
            } else {
                (
                    interactor_res.verdict,
                    interactor_res.score_ratio,
                    interactor_res.message,
                )
            }
        }
    };

    let _ = fs::remove_dir_all(&run_dir);

    let allocated_score = (test.weight as f64 / 1_000_000.0) * score_ratio;

    let exit_info = match sol_res.status {
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

    let (test_message, checker_message) = match verdict {
        Verdict::RuntimeError | Verdict::JudgeFailed => (checker_msg, None),
        _ => (None, checker_msg),
    };

    SingleTestResult {
        id: test.name.clone(),
        subtask: test.subtask_id,
        verdict,
        score: allocated_score,
        time_ms,
        wall_ms,
        memory_kb,
        exit: exit_info,
        message: test_message,
        checker: checker_message,
        diff: None,
        timing: TimingInfo {
            rejudged: false,
            confidence: if sol_res.sandbox_tier == Tier::Full {
                "high".to_string()
            } else {
                "medium".to_string()
            },
            samples_ms: vec![time_ms],
        },
        sandbox_tier: Some(sol_res.sandbox_tier),
    }
}

/// Evaluate a submitted output file for an output-only problem.
pub fn judge_output_only_test(
    sandbox: &dyn Sandbox,
    submission_dir: &Path,
    test: &TestCase,
    checker: &ResolvedChecker,
    pe_as_wa: bool,
    cancel: &CancelToken,
) -> SingleTestResult {
    let out_candidate = submission_dir.join(format!("{}.out", test.name));
    let ans_candidate = submission_dir.join(format!("{}.ans", test.name));

    let out_path = if out_candidate.is_file() {
        Some(out_candidate)
    } else if ans_candidate.is_file() {
        Some(ans_candidate)
    } else {
        None
    };

    let Some(existing_out) = out_path else {
        return SingleTestResult {
            id: test.name.clone(),
            subtask: test.subtask_id,
            verdict: Verdict::WrongAnswer,
            score: 0.0,
            time_ms: 0,
            wall_ms: 0,
            memory_kb: 0,
            exit: ExitInfo::default(),
            message: Some("Output file not found in submission".to_string()),
            checker: None,
            diff: None,
            timing: TimingInfo {
                rejudged: false,
                confidence: "high".to_string(),
                samples_ms: vec![0],
            },
            sandbox_tier: Some(Tier::None),
        };
    };

    let empty_path = PathBuf::new();
    let ans_file = test.answer_path.as_ref().unwrap_or(&empty_path);

    let (mut verdict, score_ratio, checker_msg, diff) = match checker {
        ResolvedChecker::Builtin(b) => {
            let out_bytes = fs::read(&existing_out).unwrap_or_default();
            let ans_bytes = uj_checker::read_file_bytes(ans_file).unwrap_or_default();
            let check_res = b.check(&out_bytes, &ans_bytes);
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
        ResolvedChecker::Testlib(chk_exe) => {
            let check_res = uj_checker::testlib::run_testlib_checker(
                sandbox,
                chk_exe,
                &test.input_path,
                &existing_out,
                ans_file,
                pe_as_wa,
                cancel,
            );
            (
                check_res.verdict,
                check_res.score_ratio,
                check_res.message,
                None,
            )
        }
        ResolvedChecker::Lemon {
            path: chk_exe,
            full_score,
        } => {
            let check_res = uj_checker::lemon::run_lemon_checker(
                sandbox,
                chk_exe,
                &test.input_path,
                &existing_out,
                ans_file,
                *full_score,
                cancel,
            );
            (
                check_res.verdict,
                check_res.score_ratio,
                check_res.message,
                None,
            )
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
        time_ms: 0,
        wall_ms: 0,
        memory_kb: 0,
        exit: ExitInfo::default(),
        message: None,
        checker: checker_msg,
        diff,
        timing: TimingInfo {
            rejudged: false,
            confidence: "high".to_string(),
            samples_ms: vec![0],
        },
        sandbox_tier: Some(Tier::None),
    }
}

/// Evaluate a communication problem with manager and participant instances.
pub fn judge_communication_test(
    sandbox: &dyn Sandbox,
    manager_exe: &Path,
    participant_exe: &Path,
    interpreter_cmd: Option<&[String]>,
    test: &TestCase,
    limits: &Limits,
    cancel: &CancelToken,
) -> SingleTestResult {
    let base_dir = test
        .input_path
        .parent()
        .and_then(|p| {
            if p.file_name().is_some_and(|n| n == "tests") {
                p.parent()
            } else {
                Some(p)
            }
        })
        .unwrap_or_else(|| Path::new("."));

    let box_dir = sandbox
        .create_box_dir(base_dir)
        .unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir());
    let run_dir = box_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| box_dir.clone());

    let argv_sol = if let Some(cmd) = interpreter_cmd {
        let mut modified_cmd = Vec::new();
        for (i, part) in cmd.iter().enumerate() {
            if i == 0 {
                modified_cmd.push(std::ffi::OsString::from(part));
            } else if Path::new(part).exists() {
                let script_name = Path::new(part)
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("prog.py"));
                let box_script = box_dir.join(script_name);
                let _ = std::fs::copy(part, &box_script);
                modified_cmd.push(box_script.into_os_string());
            } else {
                modified_cmd.push(std::ffi::OsString::from(part));
            }
        }
        modified_cmd
    } else {
        let authorized_exe = uj_sandbox::get_or_prepare_authorized_binary(participant_exe, base_dir)
            .unwrap_or_else(|_| participant_exe.to_path_buf());
        vec![authorized_exe.into_os_string()]
    };

    let mut mgr_limits = Limits::new(limits.cpu_time_ms * 2 + 5000, 1024 * 1024 * 1024);
    mgr_limits.wall_time_ms = limits.wall_time_ms + 2000;

    let spec_mgr = RunSpec {
        argv: vec![
            manager_exe.as_os_str().to_os_string(),
            test.input_path.as_os_str().to_os_string(),
        ],
        env: vec![],
        work_dir: run_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Inherit,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: mgr_limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let spec_sol = RunSpec {
        argv: argv_sol,
        env: vec![],
        work_dir: box_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Pipe,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: limits.clone(),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let wiring = Wiring {
        connections: vec![
            PipeConnection {
                from_proc: 0,
                to_proc: 1,
            },
            PipeConnection {
                from_proc: 1,
                to_proc: 0,
            },
        ],
    };

    let start_instant = Instant::now();
    let group_res = sandbox.run_group(&[spec_mgr, spec_sol], &wiring, cancel);
    let wall_ms = start_instant.elapsed().as_millis() as u64;

    let results = match group_res {
        Ok(r) if r.len() >= 2 => r,
        Ok(_) | Err(_) => {
            let _ = fs::remove_dir_all(&run_dir);
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some("Communication run_group failed".to_string()),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(sandbox.tier()),
            };
        }
    };

    let mgr_res = &results[0];
    let sol_res = &results[1];

    let time_ms = (sol_res.cpu_time_us / 1000).max(1);
    let memory_kb = sol_res.memory_peak_bytes / 1024;

    let mgr_stdout = mgr_res
        .stdout
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.bytes).trim().to_string())
        .unwrap_or_default();
    let mgr_stderr = String::from_utf8_lossy(&mgr_res.stderr_tail)
        .trim()
        .to_string();

    let score_float: f64 = mgr_stdout
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0.0);
    let score_ratio = if score_float > 1.0 {
        score_float / 100.0
    } else {
        score_float
    }
    .clamp(0.0, 1.0);

    let verdict = if score_ratio >= 1.0 {
        Verdict::Accepted
    } else if score_ratio > 0.0 {
        Verdict::PartiallyCorrect
    } else {
        Verdict::WrongAnswer
    };

    let allocated_score = (test.weight as f64 / 1_000_000.0) * score_ratio;

    // Clean up isolated run directory
    let _ = fs::remove_dir_all(&run_dir);

    SingleTestResult {
        id: test.name.clone(),
        subtask: test.subtask_id,
        verdict,
        score: allocated_score,
        time_ms,
        wall_ms,
        memory_kb,
        exit: ExitInfo::default(),
        message: None,
        checker: Some(if mgr_stderr.is_empty() {
            mgr_stdout
        } else {
            mgr_stderr
        }),
        diff: None,
        timing: Default::default(),
        sandbox_tier: Some(sol_res.sandbox_tier),
    }
}

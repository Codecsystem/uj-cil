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
    prepared_exe: Option<&Path>,
    test: &TestCase,
    limits: &Limits,
    checker: &ResolvedChecker,
    pe_as_wa: bool,
    cpu_affinity: Option<usize>,
    io: Option<&uj_core::config::IoConfig>,
    cancel: &CancelToken,
    custom_box: Option<&Path>,
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

    let (box_dir, run_dir, is_reused_box) = if let Some(cb) = custom_box {
        let _ = uj_sandbox::clean_box_dir_contents(cb);
        (cb.to_path_buf(), None, true)
    } else {
        let bd = match sandbox.create_box_dir(base_dir) {
            Ok(d) => d,
            Err(e) => {
                return SingleTestResult {
                    id: test.name.clone(),
                    subtask: test.subtask_id,
                    verdict: Verdict::JudgeFailed,
                    score: 0.0,
                    time_ms: 0,
                    wall_ms: 0,
                    memory_kb: 0,
                    exit: ExitInfo::default(),
                    message: Some(format!("Failed to create sandbox box directory: {e}")),
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: Some(sandbox.tier()),
                };
            }
        };
        let rd = bd.parent().map(|p| p.to_path_buf());
        (bd, rd, false)
    };

    let is_file_io = io.is_some_and(|i| {
        i.mode == uj_core::config::IoMode::File || i.input_name.is_some() || i.output_name.is_some()
    });
    let in_name = io
        .and_then(|i| i.input_name.as_deref())
        .unwrap_or("input.txt");
    let out_name = io
        .and_then(|i| i.output_name.as_deref())
        .unwrap_or("output.txt");

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
    } else if let Some(p) = prepared_exe {
        vec![p.to_path_buf().into_os_string()]
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
    // Tracks retry attempts per binary to handle concurrent -j execution under Defender jitter,
    // while identifying genuinely slow binaries to avoid repeated 10s stalls.
    #[cfg(windows)]
    #[allow(clippy::type_complexity)]
    static RETRY_TRACKER: std::sync::LazyLock<
        parking_lot::Mutex<(
            std::collections::HashSet<PathBuf>,
            std::collections::HashMap<PathBuf, u32>,
        )>,
    > = std::sync::LazyLock::new(|| {
        parking_lot::Mutex::new((
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
        ))
    });

    #[cfg(windows)]
    if let Ok(ref res) = run_res {
        let cpu_ms = res.cpu_time_us / 1000;
        let threshold_ms = ((limits.cpu_time_ms / 10).max(50)).min(limits.cpu_time_ms);
        if res.killed_by == Some(KillReason::WallLimit)
            && cpu_ms < threshold_ms
            && !cancel.is_cancelled()
        {
            let should_retry = {
                let mut state = RETRY_TRACKER.lock();
                if state.0.contains(exe_path) {
                    false
                } else {
                    let count = state.1.entry(exe_path.to_path_buf()).or_insert(0);
                    if *count < 3 {
                        *count += 1;
                        true
                    } else {
                        false
                    }
                }
            };
            if should_retry {
                let mut retry_spec = spec.clone();
                retry_spec.limits.wall_time_ms = retry_spec.limits.wall_time_ms.max(10000);
                let retry_start = Instant::now();
                if let Ok(retry_res) = sandbox.run(&retry_spec, cancel) {
                    if retry_res.killed_by == Some(KillReason::WallLimit) {
                        let mut state = RETRY_TRACKER.lock();
                        state.0.insert(exe_path.to_path_buf());
                    }
                    run_res = Ok(retry_res);
                    duration_ms = retry_start.elapsed().as_millis() as u64;
                }
            }
        }
    }

    let res = match run_res {
        Ok(r) => r,
        Err(e) => {
            if is_reused_box {
                let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
            } else if let Some(ref rd) = run_dir {
                let _ = fs::remove_dir_all(rd);
            }
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
                // 1. Resolve contestant output bytes (enforcing file_io vs stdout and bounds)
                let output_bytes_res = if is_file_io {
                    let out_path = box_dir.join(out_name);
                    if !out_path.exists() {
                        Err((
                            Verdict::WrongAnswer,
                            0.0,
                            Some(format!("Output file '{out_name}' was not created")),
                            None,
                            None,
                        ))
                    } else {
                        match uj_core::read_artifact_bounded(&out_path, spec.limits.output_bytes) {
                            Ok(art) => Ok(art.bytes),
                            Err(uj_core::ReadArtifactError::TooLarge { size, limit }) => Err((
                                Verdict::OutputLimitExceeded,
                                0.0,
                                Some(format!(
                                    "Output file '{out_name}' size ({size} bytes) exceeds limit ({limit} bytes)"
                                )),
                                None,
                                None,
                            )),
                            Err(e) => Err((
                                Verdict::JudgeFailed,
                                0.0,
                                Some(format!("Failed to read output file '{out_name}': {e}")),
                                None,
                                None,
                            )),
                        }
                    }
                } else if let Some(ref out) = res.stdout {
                    if out.truncated || res.killed_by == Some(KillReason::OutputLimit) {
                        Err((
                            Verdict::OutputLimitExceeded,
                            0.0,
                            Some("Standard output exceeded limit".to_string()),
                            None,
                            None,
                        ))
                    } else {
                        Ok(out.bytes.clone())
                    }
                } else {
                    Ok(Vec::new())
                };

                match output_bytes_res {
                    Err(verdict_tuple) => verdict_tuple,
                    Ok(stdout_bytes) => {
                        // 2. Validate and read answer file with bounded reader
                        match &test.answer_path {
                            None => (
                                Verdict::JudgeFailed,
                                0.0,
                                Some("No answer file specified for test case".to_string()),
                                None,
                                None,
                            ),
                            Some(ans_path) => {
                                let ans_limit = std::cmp::max(
                                    spec.limits.output_bytes.saturating_mul(2),
                                    Limits::DEFAULT_OUTPUT_BYTES,
                                );
                                #[allow(clippy::type_complexity)]
                                static ANSWER_FILE_CACHE: std::sync::LazyLock<
                                    parking_lot::Mutex<
                                        std::collections::HashMap<
                                            PathBuf,
                                            (
                                                std::time::SystemTime,
                                                u64,
                                                std::sync::Arc<uj_checker::FileBytes>,
                                            ),
                                        >,
                                    >,
                                > = std::sync::LazyLock::new(|| {
                                    parking_lot::Mutex::new(std::collections::HashMap::new())
                                });

                                #[allow(clippy::type_complexity, clippy::result_large_err)]
                                let ans_bytes_res = (|| -> Result<
                                    std::sync::Arc<uj_checker::FileBytes>,
                                    (
                                        Verdict,
                                        f64,
                                        Option<String>,
                                        Option<String>,
                                        Option<DiffSummary>,
                                    ),
                                > {
                                    let meta = match fs::metadata(ans_path) {
                                        Ok(m) => m,
                                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                            return Err((
                                                Verdict::JudgeFailed,
                                                0.0,
                                                Some(format!(
                                                    "Answer file not found: {}",
                                                    ans_path.display()
                                                )),
                                                None,
                                                None,
                                            ));
                                        }
                                        Err(e)
                                            if e.kind() == std::io::ErrorKind::PermissionDenied =>
                                        {
                                            return Err((
                                                Verdict::JudgeFailed,
                                                0.0,
                                                Some(format!(
                                                    "Permission denied reading answer file: {}",
                                                    ans_path.display()
                                                )),
                                                None,
                                                None,
                                            ));
                                        }
                                        Err(e) => {
                                            return Err((
                                                Verdict::JudgeFailed,
                                                0.0,
                                                Some(format!("Failed to read answer file: {e}")),
                                                None,
                                                None,
                                            ));
                                        }
                                    };
                                    let len = meta.len();
                                    if len > ans_limit {
                                        return Err((
                                            Verdict::JudgeFailed,
                                            0.0,
                                            Some(format!(
                                                "Answer file exceeded limit: {len} bytes > {ans_limit} bytes"
                                            )),
                                            None,
                                            None,
                                        ));
                                    }
                                    let mtime = meta
                                        .modified()
                                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                                    let mut cache = ANSWER_FILE_CACHE.lock();
                                    if let Some((c_mtime, c_len, c_bytes)) = cache.get(ans_path)
                                        && *c_mtime == mtime
                                        && *c_len == len
                                    {
                                        return Ok(c_bytes.clone());
                                    }
                                    let fb = uj_checker::read_file_bytes(ans_path).map_err(|e| {
                                        (
                                            Verdict::JudgeFailed,
                                            0.0,
                                            Some(format!("Failed to read answer file: {e}")),
                                            None,
                                            None,
                                        )
                                    })?;
                                    let arc = std::sync::Arc::new(fb);
                                    cache.insert(ans_path.to_path_buf(), (mtime, len, arc.clone()));
                                    Ok(arc)
                                })();

                                match ans_bytes_res {
                                    Err(fail_tuple) => fail_tuple,
                                    Ok(ans_artifact) => {
                                        let ans_slice = &ans_artifact[..];

                                        match checker {
                                            ResolvedChecker::Builtin(b) => {
                                                let check_res = b.check(&stdout_bytes, ans_slice);
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
                                                let out_file = box_dir.join("uj_out.txt");
                                                let _ = fs::write(&out_file, &stdout_bytes);

                                                let check_res =
                                                    uj_checker::testlib::run_testlib_checker(
                                                        sandbox,
                                                        chk_exe,
                                                        &test.input_path,
                                                        &out_file,
                                                        ans_path,
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
                                                let out_file = box_dir.join("uj_out.txt");
                                                let _ = fs::write(&out_file, &stdout_bytes);

                                                let check_res =
                                                    uj_checker::lemon::run_lemon_checker(
                                                        sandbox,
                                                        chk_exe,
                                                        &test.input_path,
                                                        &out_file,
                                                        ans_path,
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
                            }
                        }
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
    if is_reused_box {
        let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
    } else if let Some(ref rd) = run_dir {
        let _ = fs::remove_dir_all(rd);
    }

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

/// Compute deterministic topological order of subtask configurations.
fn topological_subtask_order(subtasks: &[uj_core::config::SubtaskConfig]) -> Vec<usize> {
    let n = subtasks.len();
    if n <= 1 {
        return (0..n).collect();
    }

    let id_to_idx: std::collections::HashMap<u32, usize> = subtasks
        .iter()
        .enumerate()
        .map(|(idx, s)| (s.id, idx))
        .collect();

    let mut in_degree: Vec<usize> = vec![0; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (u_idx, st) in subtasks.iter().enumerate() {
        for &dep_id in &st.depends {
            if let Some(&dep_idx) = id_to_idx.get(&dep_id)
                && dep_idx != u_idx
            {
                in_degree[u_idx] += 1;
                dependents[dep_idx].push(u_idx);
            }
        }
    }

    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<(u32, usize)>> =
        std::collections::BinaryHeap::new();

    for (idx, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            ready.push(std::cmp::Reverse((subtasks[idx].id, idx)));
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(std::cmp::Reverse((_, u_idx))) = ready.pop() {
        order.push(u_idx);
        for &v_idx in &dependents[u_idx] {
            in_degree[v_idx] -= 1;
            if in_degree[v_idx] == 0 {
                ready.push(std::cmp::Reverse((subtasks[v_idx].id, v_idx)));
            }
        }
    }

    if order.len() < n {
        for idx in 0..n {
            if !order.contains(&idx) {
                order.push(idx);
            }
        }
    }

    order
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

    let mut subtask_results_map: std::collections::HashMap<u32, SubtaskResult> =
        std::collections::HashMap::new();

    // Evaluate subtasks in deterministic topological order
    let topo_indices = topological_subtask_order(&config.subtasks);

    for &idx in &topo_indices {
        let st_cfg = &config.subtasks[idx];
        let tests = subtask_map.remove(&st_cfg.id).unwrap_or_default();
        let test_ids: Vec<String> = tests.iter().map(|t| t.id.clone()).collect();
        let st_max_fixed = (st_cfg.score * 10000.0).round() as i64;

        // Check dependencies - all dependencies are already evaluated and in subtask_results_map
        let mut dep_failed = false;
        for &dep_id in &st_cfg.depends {
            if let Some(dep_res) = subtask_results_map.get(&dep_id) {
                if dep_res.score < dep_res.max_score || dep_res.verdict != Verdict::Accepted {
                    dep_failed = true;
                    break;
                }
            } else {
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

            // Verdict resolution:
            // Severe errors (SE, CE, RF, MLE, OLE, TLE, RE) must NEVER be masked by PartiallyCorrect!
            let final_verdict = if score_fixed >= st_max_fixed
                || worst.priority() > Verdict::WrongAnswer.priority()
            {
                worst
            } else if score_fixed > 0 {
                Verdict::PartiallyCorrect
            } else {
                worst
            };

            (final_verdict, score_fixed)
        };

        let st_score = st_score_fixed as f64 / 10000.0;

        subtask_results_map.insert(
            st_cfg.id,
            SubtaskResult {
                id: st_cfg.id,
                score: st_score,
                max_score: st_cfg.score,
                verdict: st_verdict,
                test_ids,
            },
        );
    }

    let mut subtask_results = Vec::new();
    let mut total_score_fixed: i64 = 0;
    let mut worst_verdict = Verdict::Accepted;

    // Assemble subtasks in config declaration order
    for st_cfg in &config.subtasks {
        if let Some(res) = subtask_results_map.remove(&st_cfg.id) {
            worst_verdict = worst_verdict.merge(res.verdict);
            total_score_fixed += (res.score * 10000.0).round() as i64;
            subtask_results.push(res);
        }
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
    } else if worst_verdict.priority() > Verdict::WrongAnswer.priority() {
        // Severe errors (JudgeFailed, TLE, MLE, OLE, RE, etc.) must NEVER be masked by PC!
        worst_verdict
    } else if total_score > 0.0 && worst_verdict.priority() <= Verdict::WrongAnswer.priority() {
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
    prepared_exe: Option<&Path>,
    interactor_exe: &Path,
    test: &TestCase,
    limits: &Limits,
    checker: Option<&ResolvedChecker>,
    pe_as_wa: bool,
    cancel: &CancelToken,
    custom_box: Option<&Path>,
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

    let (box_dir, run_dir, is_reused_box) = if let Some(cb) = custom_box {
        let _ = uj_sandbox::clean_box_dir_contents(cb);
        (cb.to_path_buf(), None, true)
    } else {
        let bd = match sandbox.create_box_dir(base_dir) {
            Ok(d) => d,
            Err(e) => {
                return SingleTestResult {
                    id: test.name.clone(),
                    subtask: test.subtask_id,
                    verdict: Verdict::JudgeFailed,
                    score: 0.0,
                    time_ms: 0,
                    wall_ms: 0,
                    memory_kb: 0,
                    exit: ExitInfo::default(),
                    message: Some(format!("Failed to create sandbox box directory: {e}")),
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: Some(sandbox.tier()),
                };
            }
        };
        let rd = bd.parent().map(|p| p.to_path_buf());
        (bd, rd, false)
    };

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
    } else if let Some(p) = prepared_exe {
        vec![p.to_path_buf().into_os_string()]
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

    // CRITICAL (P0-01): Create a dedicated trusted directory OUTSIDE the contestant's box_dir.
    // The contestant's AppContainer on Windows and userns on Linux only have write access to box_dir.
    // Placing judge results outside box_dir prevents contestant code from spoofing or modifying XML results.
    let trusted_dir = box_dir
        .parent()
        .unwrap_or(&box_dir)
        .join(format!("trusted_int_{pid}_{time_now}"));
    let _ = fs::create_dir_all(&trusted_dir);

    let tout_file = trusted_dir.join(format!("interact_tout_{pid}_{time_now}.txt"));
    let result_file = trusted_dir.join(format!("interact_res_{pid}_{time_now}.xml"));

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
        work_dir: box_dir.clone(),
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
            let _ = fs::remove_dir_all(&trusted_dir);
            if is_reused_box {
                let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
            } else if let Some(ref rd) = run_dir {
                let _ = fs::remove_dir_all(rd);
            }
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
                timing: TimingInfo::default(),
                sandbox_tier: Some(sandbox.tier()),
            };
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&trusted_dir);
            if is_reused_box {
                let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
            } else if let Some(ref rd) = run_dir {
                let _ = fs::remove_dir_all(rd);
            }
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
                        let ans_bytes = uj_checker::read_file_bytes(ans_file).ok();
                        let ans_slice = ans_bytes.as_deref().unwrap_or(b"");
                        let check_res = b.check(&tout_bytes, ans_slice);
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

    let _ = fs::remove_dir_all(&trusted_dir);
    if is_reused_box {
        let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
    } else if let Some(ref rd) = run_dir {
        let _ = fs::remove_dir_all(rd);
    }

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

    // P0-03: Validate answer file existence
    let ans_path = match &test.answer_path {
        Some(p) if p.is_file() => p,
        Some(p) => {
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: 0,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some(format!("Answer file not found: {}", p.display())),
                checker: None,
                diff: None,
                timing: TimingInfo {
                    rejudged: false,
                    confidence: "high".to_string(),
                    samples_ms: vec![0],
                },
                sandbox_tier: Some(Tier::None),
            };
        }
        None => {
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                time_ms: 0,
                wall_ms: 0,
                memory_kb: 0,
                exit: ExitInfo::default(),
                message: Some("No answer file specified for test case".to_string()),
                checker: None,
                diff: None,
                timing: TimingInfo {
                    rejudged: false,
                    confidence: "high".to_string(),
                    samples_ms: vec![0],
                },
                sandbox_tier: Some(Tier::None),
            };
        }
    };

    // P0-03: Enforce output size limit on submitted output file
    let out_artifact =
        match uj_core::read_artifact_bounded(&existing_out, Limits::DEFAULT_OUTPUT_BYTES) {
            Ok(art) => art,
            Err(uj_core::ReadArtifactError::TooLarge { size, limit, .. }) => {
                return SingleTestResult {
                    id: test.name.clone(),
                    subtask: test.subtask_id,
                    verdict: Verdict::OutputLimitExceeded,
                    score: 0.0,
                    time_ms: 0,
                    wall_ms: 0,
                    memory_kb: 0,
                    exit: ExitInfo::default(),
                    message: Some(format!(
                        "Submission output file exceeded limit ({size} > {limit} bytes)"
                    )),
                    checker: None,
                    diff: None,
                    timing: TimingInfo {
                        rejudged: false,
                        confidence: "high".to_string(),
                        samples_ms: vec![0],
                    },
                    sandbox_tier: Some(Tier::None),
                };
            }
            Err(e) => {
                return SingleTestResult {
                    id: test.name.clone(),
                    subtask: test.subtask_id,
                    verdict: Verdict::JudgeFailed,
                    score: 0.0,
                    time_ms: 0,
                    wall_ms: 0,
                    memory_kb: 0,
                    exit: ExitInfo::default(),
                    message: Some(format!("Failed to read submission output file: {e}")),
                    checker: None,
                    diff: None,
                    timing: TimingInfo {
                        rejudged: false,
                        confidence: "high".to_string(),
                        samples_ms: vec![0],
                    },
                    sandbox_tier: Some(Tier::None),
                };
            }
        };

    let (mut verdict, score_ratio, checker_msg, diff) = match checker {
        ResolvedChecker::Builtin(b) => {
            let ans_artifact =
                match uj_core::read_artifact_bounded(ans_path, Limits::DEFAULT_OUTPUT_BYTES * 2) {
                    Ok(art) => art,
                    Err(e) => {
                        return SingleTestResult {
                            id: test.name.clone(),
                            subtask: test.subtask_id,
                            verdict: Verdict::JudgeFailed,
                            score: 0.0,
                            time_ms: 0,
                            wall_ms: 0,
                            memory_kb: 0,
                            exit: ExitInfo::default(),
                            message: Some(format!("Failed to read answer file: {e}")),
                            checker: None,
                            diff: None,
                            timing: TimingInfo {
                                rejudged: false,
                                confidence: "high".to_string(),
                                samples_ms: vec![0],
                            },
                            sandbox_tier: Some(Tier::None),
                        };
                    }
                };
            let check_res = b.check(&out_artifact.bytes, &ans_artifact.bytes);
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
                ans_path,
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
                ans_path,
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
#[allow(clippy::too_many_arguments)]
pub fn judge_communication_test(
    sandbox: &dyn Sandbox,
    manager_exe: &Path,
    participant_exe: &Path,
    interpreter_cmd: Option<&[String]>,
    prepared_exe: Option<&Path>,
    test: &TestCase,
    limits: &Limits,
    cancel: &CancelToken,
    custom_box: Option<&Path>,
    num_processes: usize,
    topology: Option<&str>,
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

    let (box_dir, run_dir, is_reused_box) = if let Some(cb) = custom_box {
        let _ = uj_sandbox::clean_box_dir_contents(cb);
        (cb.to_path_buf(), None, true)
    } else {
        let bd = match sandbox.create_box_dir(base_dir) {
            Ok(d) => d,
            Err(e) => {
                return SingleTestResult {
                    id: test.name.clone(),
                    subtask: test.subtask_id,
                    verdict: Verdict::JudgeFailed,
                    score: 0.0,
                    time_ms: 0,
                    wall_ms: 0,
                    memory_kb: 0,
                    exit: ExitInfo::default(),
                    message: Some(format!("Failed to create sandbox box directory: {e}")),
                    checker: None,
                    diff: None,
                    timing: Default::default(),
                    sandbox_tier: Some(sandbox.tier()),
                };
            }
        };
        let rd = bd.parent().map(|p| p.to_path_buf());
        (bd, rd, false)
    };

    let trusted_dir = tempfile::tempdir().ok();
    let score_file = trusted_dir.as_ref().map(|d| d.path().join("score.txt"));

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
    } else if let Some(p) = prepared_exe {
        vec![p.to_path_buf().into_os_string()]
    } else {
        let authorized_exe =
            uj_sandbox::get_or_prepare_authorized_binary(participant_exe, base_dir)
                .unwrap_or_else(|_| participant_exe.to_path_buf());
        vec![authorized_exe.into_os_string()]
    };

    let mut mgr_limits = Limits::new(limits.cpu_time_ms * 2 + 5000, 1024 * 1024 * 1024);
    mgr_limits.wall_time_ms = limits.wall_time_ms + 2000;

    let mut mgr_argv = vec![
        manager_exe.as_os_str().to_os_string(),
        test.input_path.as_os_str().to_os_string(),
    ];
    let mut mgr_env = Vec::new();
    if let Some(ref sf) = score_file {
        mgr_argv.push(sf.as_os_str().to_os_string());
        mgr_env.push((
            std::ffi::OsString::from("UJ_SCORE_FILE"),
            sf.as_os_str().to_os_string(),
        ));
    }

    let spec_mgr = RunSpec {
        argv: mgr_argv,
        env: mgr_env,
        work_dir: box_dir.clone(),
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

    let num_participants = num_processes.max(1);
    let mut specs = vec![spec_mgr];
    for _ in 0..num_participants {
        specs.push(spec_sol.clone());
    }

    let wiring = if num_participants == 1 {
        Wiring {
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
        }
    } else if topology == Some("ring") || topology == Some("pipeline") {
        let mut connections = Vec::new();
        connections.push(PipeConnection {
            from_proc: 0,
            to_proc: 1,
        });
        for p in 1..num_participants {
            connections.push(PipeConnection {
                from_proc: p,
                to_proc: p + 1,
            });
        }
        connections.push(PipeConnection {
            from_proc: num_participants,
            to_proc: 0,
        });
        Wiring { connections }
    } else {
        if is_reused_box {
            let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
        } else if let Some(ref rd) = run_dir {
            let _ = fs::remove_dir_all(rd);
        }
        return SingleTestResult {
            id: test.name.clone(),
            subtask: test.subtask_id,
            verdict: Verdict::JudgeFailed,
            score: 0.0,
            time_ms: 0,
            wall_ms: 0,
            memory_kb: 0,
            exit: ExitInfo::default(),
            message: Some(format!(
                "Unsupported communication topology '{:?}' for {} participants",
                topology, num_participants
            )),
            checker: None,
            diff: None,
            timing: Default::default(),
            sandbox_tier: None,
        };
    };

    let start_instant = Instant::now();
    let group_res = sandbox.run_group(&specs, &wiring, cancel);
    let wall_ms = start_instant.elapsed().as_millis() as u64;

    let results = match group_res {
        Ok(r) if r.len() > num_participants => r,
        Ok(_) | Err(_) => {
            if is_reused_box {
                let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
            } else if let Some(ref rd) = run_dir {
                let _ = fs::remove_dir_all(rd);
            }
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
    let sol_results = &results[1..];

    let max_time_us = sol_results.iter().map(|r| r.cpu_time_us).max().unwrap_or(0);
    let time_ms = (max_time_us / 1000).max(1);
    let max_peak_bytes = sol_results
        .iter()
        .map(|r| r.memory_peak_bytes)
        .max()
        .unwrap_or(0);
    let memory_kb = max_peak_bytes / 1024;
    let participant_tier = sol_results[0].sandbox_tier;

    let cleanup = || {
        if is_reused_box {
            let _ = uj_sandbox::clean_box_dir_contents(&box_dir);
        } else if let Some(ref rd) = run_dir {
            let _ = fs::remove_dir_all(rd);
        }
    };

    if cancel.is_cancelled() {
        cleanup();
        return SingleTestResult {
            id: test.name.clone(),
            subtask: test.subtask_id,
            verdict: Verdict::JudgeFailed,
            score: 0.0,
            time_ms,
            wall_ms,
            memory_kb,
            exit: ExitInfo::default(),
            message: Some("Cancelled by user".to_string()),
            checker: None,
            diff: None,
            timing: Default::default(),
            sandbox_tier: Some(participant_tier),
        };
    }

    // Check if any participant violated limits or crashed
    for (i, p_res) in sol_results.iter().enumerate() {
        if p_res.killed_by == Some(KillReason::CpuLimit)
            || p_res.killed_by == Some(KillReason::WallLimit)
        {
            cleanup();
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::TimeLimitExceeded,
                score: 0.0,
                time_ms,
                wall_ms,
                memory_kb,
                exit: ExitInfo::default(),
                message: Some(format!("Participant {} exceeded time limit", i + 1)),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(participant_tier),
            };
        }
        if p_res.killed_by == Some(KillReason::MemoryLimit) || p_res.oom_killed {
            cleanup();
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::MemoryLimitExceeded,
                score: 0.0,
                time_ms,
                wall_ms,
                memory_kb,
                exit: ExitInfo::default(),
                message: Some(format!("Participant {} exceeded memory limit", i + 1)),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(participant_tier),
            };
        }
        if p_res.killed_by == Some(KillReason::OutputLimit) {
            cleanup();
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::OutputLimitExceeded,
                score: 0.0,
                time_ms,
                wall_ms,
                memory_kb,
                exit: ExitInfo::default(),
                message: Some(format!("Participant {} exceeded output limit", i + 1)),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(participant_tier),
            };
        }
        if !p_res.status.is_success() {
            let stderr_msg = String::from_utf8_lossy(&p_res.stderr_tail)
                .trim()
                .to_string();
            cleanup();
            return SingleTestResult {
                id: test.name.clone(),
                subtask: test.subtask_id,
                verdict: Verdict::RuntimeError,
                score: 0.0,
                time_ms,
                wall_ms,
                memory_kb,
                exit: ExitInfo::default(),
                message: Some(if stderr_msg.is_empty() {
                    format!(
                        "Participant {} failed with status {:?}",
                        i + 1,
                        p_res.status
                    )
                } else {
                    stderr_msg
                }),
                checker: None,
                diff: None,
                timing: Default::default(),
                sandbox_tier: Some(participant_tier),
            };
        }
    }

    // Parse score from score_file
    let mut score_float: Option<f64> = None;
    if let Some(ref sf) = score_file
        && let Ok(content) = fs::read_to_string(sf)
        && let Some(first_line) = content.lines().next()
        && let Ok(val) = first_line.trim().parse::<f64>()
        && val.is_finite()
    {
        score_float = Some(val);
    }

    // Fallback: check manager stderr
    let mgr_stderr = String::from_utf8_lossy(&mgr_res.stderr_tail)
        .trim()
        .to_string();
    if score_float.is_none() {
        for line in mgr_stderr.lines() {
            let trimmed = line.trim();
            if let Ok(val) = trimmed.parse::<f64>()
                && val.is_finite()
            {
                score_float = Some(val);
                break;
            }
        }
    }

    // Fallback: check manager stdout
    let mgr_stdout = mgr_res
        .stdout
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.bytes).trim().to_string())
        .unwrap_or_default();
    if score_float.is_none()
        && let Some(line) = mgr_stdout.lines().next()
        && let Ok(val) = line.trim().parse::<f64>()
        && val.is_finite()
    {
        score_float = Some(val);
    }

    // If manager crashed or timed out and produced no score: JudgeFailed
    if !mgr_res.status.is_success() && score_float.is_none() {
        cleanup();
        return SingleTestResult {
            id: test.name.clone(),
            subtask: test.subtask_id,
            verdict: Verdict::JudgeFailed,
            score: 0.0,
            time_ms,
            wall_ms,
            memory_kb,
            exit: ExitInfo::default(),
            message: Some(if mgr_stderr.is_empty() {
                format!("Manager failed with status {:?}", mgr_res.status)
            } else {
                mgr_stderr
            }),
            checker: None,
            diff: None,
            timing: Default::default(),
            sandbox_tier: Some(participant_tier),
        };
    }

    let parsed_score = score_float.unwrap_or_else(|| {
        if mgr_res.status.is_success() {
            1.0
        } else {
            0.0
        }
    });

    let score_ratio = if parsed_score > 1.0 {
        parsed_score / 100.0
    } else {
        parsed_score
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
    cleanup();

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
        sandbox_tier: Some(participant_tier),
    }
}

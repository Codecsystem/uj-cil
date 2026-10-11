//! Multi-threaded test case scheduler, LPT heuristic queue, and event dispatch.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;

use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{CheckerType, ProblemConfig, ProblemType};
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{JudgeEvent, SingleTestResult, SubmissionJudgeReport};
use uj_sandbox::{CancelToken, create_sandbox};

use crate::compiler::CompileResult;
pub use crate::judge::JudgeOptions;
use crate::judge::{
    ResolvedChecker, aggregate_submission_report, judge_communication_test, judge_interactive_test,
    judge_output_only_test, judge_single_test,
};
use crate::topology::Topology;

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Sandbox error: {0}")]
    Sandbox(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Scheduler {
    pub options: JudgeOptions,
    pub session: crate::compiler::CompileSession,
}

impl Scheduler {
    pub fn new(options: JudgeOptions) -> Self {
        Self {
            options,
            session: crate::compiler::CompileSession::new(),
        }
    }

    pub fn with_session(options: JudgeOptions, session: crate::compiler::CompileSession) -> Self {
        Self { options, session }
    }

    /// Run full judging pipeline for a submission against a problem.
    #[allow(clippy::too_many_arguments)]
    pub fn judge_submission(
        &self,
        problem_dir: &Path,
        problem_config: &ProblemConfig,
        compiled: &CompileResult,
        submission_name: &str,
        tests: Vec<TestCase>,
        user_jobs: Option<usize>,
        event_sender: Option<Sender<JudgeEvent>>,
        cancel: &CancelToken,
    ) -> Result<SubmissionJudgeReport, SchedulerError> {
        // Pre-validate sandbox initialization and required tier
        let prechecked_sandbox =
            create_sandbox(&self.options.sandbox_mode, self.options.require_tier).map_err(|e| {
                SchedulerError::Sandbox(format!("Failed to initialize sandbox: {e}"))
            })?;
        let mut prechecked_sandbox = Some(prechecked_sandbox);

        let limits = problem_config
            .to_limits()
            .map_err(|e| SchedulerError::Config(e.to_string()))?;
        let topology = Topology::detect(limits.memory_bytes, user_jobs);

        let pe_as_wa = problem_config.checker.pe_as_wa;

        let compile_session = &self.session;

        let resolved_checker = match problem_config.checker.checker_type {
            CheckerType::Builtin => {
                let checker_name = problem_config.checker.name.as_deref().unwrap_or("wcmp");
                let b = BuiltinCheckerType::parse(checker_name).unwrap_or(BuiltinCheckerType::Wcmp);
                ResolvedChecker::Builtin(b)
            }
            CheckerType::Testlib => {
                let chk_path = problem_config
                    .checker
                    .path
                    .as_ref()
                    .map(|p| {
                        if p.is_absolute() {
                            p.clone()
                        } else {
                            problem_dir.join(p)
                        }
                    })
                    .ok_or_else(|| {
                        SchedulerError::Config(
                            "testlib checker path not specified in problem.toml".to_string(),
                        )
                    })?;

                let bin_path = if chk_path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
                {
                    chk_path
                } else {
                    let compiled_chk = compile_session.compile(&chk_path, false).map_err(|e| {
                        SchedulerError::Config(format!("Failed to compile testlib checker: {e}"))
                    })?;
                    compiled_chk.binary_path
                };
                ResolvedChecker::Testlib(bin_path)
            }
            CheckerType::Lemon => {
                let chk_path = problem_config
                    .checker
                    .path
                    .as_ref()
                    .map(|p| {
                        if p.is_absolute() {
                            p.clone()
                        } else {
                            problem_dir.join(p)
                        }
                    })
                    .ok_or_else(|| {
                        SchedulerError::Config(
                            "Lemon SPJ path not specified in problem.toml".to_string(),
                        )
                    })?;

                let bin_path = if chk_path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
                {
                    chk_path
                } else {
                    let compiled_chk = compile_session.compile(&chk_path, false).map_err(|e| {
                        SchedulerError::Config(format!("Failed to compile Lemon SPJ: {e}"))
                    })?;
                    compiled_chk.binary_path
                };
                ResolvedChecker::Lemon {
                    path: bin_path,
                    full_score: 100.0,
                }
            }
        };

        let interactor_bin = if problem_config.problem_type == ProblemType::Interactive {
            let int_path = problem_config
                .interactor
                .as_ref()
                .map(|p| {
                    if p.is_absolute() {
                        p.clone()
                    } else {
                        problem_dir.join(p)
                    }
                })
                .ok_or_else(|| {
                    SchedulerError::Config(
                        "interactor path not specified in problem.toml".to_string(),
                    )
                })?;

            let bin_path = if int_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            {
                int_path
            } else {
                let compiled_int = compile_session.compile(&int_path, false).map_err(|e| {
                    SchedulerError::Config(format!("Failed to compile interactor: {e}"))
                })?;
                compiled_int.binary_path
            };
            Some(bin_path)
        } else {
            None
        };

        let manager_bin = if problem_config.problem_type == ProblemType::Communication {
            let mgr_path = problem_config
                .manager
                .as_ref()
                .map(|p| {
                    if p.is_absolute() {
                        p.clone()
                    } else {
                        problem_dir.join(p)
                    }
                })
                .ok_or_else(|| {
                    SchedulerError::Config("manager path not specified in problem.toml".to_string())
                })?;

            let bin_path = if mgr_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            {
                mgr_path
            } else {
                let compiled_mgr = compile_session.compile(&mgr_path, false).map_err(|e| {
                    SchedulerError::Config(format!("Failed to compile manager: {e}"))
                })?;
                compiled_mgr.binary_path
            };
            Some(bin_path)
        } else {
            None
        };

        // Prepare submission binary content once for native submissions
        let base_dir_path = tests
            .first()
            .map(|t| {
                t.input_path
                    .parent()
                    .and_then(|p| {
                        if p.file_name().is_some_and(|n| n == "tests") {
                            p.parent()
                        } else {
                            Some(p)
                        }
                    })
                    .unwrap_or(problem_dir)
            })
            .unwrap_or(problem_dir)
            .to_path_buf();

        let prepared_exe = if compiled.interpreter_cmd.is_none()
            && problem_config.problem_type != ProblemType::OutputOnly
        {
            match uj_sandbox::prepare_binary_content(&compiled.binary_path, &base_dir_path) {
                Ok(p) => Some(p),
                Err(e) => {
                    return Err(SchedulerError::Sandbox(format!(
                        "Failed to prepare submission binary: {e}"
                    )));
                }
            }
        } else {
            None
        };

        // Sort test indices by input file size descending (LPT heuristic)
        use std::cmp::Reverse;
        let mut sorted_indices: Vec<usize> = (0..tests.len()).collect();
        sorted_indices.sort_by_cached_key(|&idx| {
            Reverse(
                fs::metadata(&tests[idx].input_path)
                    .map(|m| m.len())
                    .unwrap_or(0),
            )
        });

        let total_tests = sorted_indices.len();

        // Shared state for fail-fast subtask tracking
        let failed_subtasks: Arc<Mutex<std::collections::HashSet<u32>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));

        let (task_tx, task_rx) = unbounded::<TestCase>();
        let (result_tx, result_rx) = bounded::<SingleTestResult>(total_tests.max(1));

        let slot_count = topology.slots.len().min(total_tests);
        let mut worker_handles = Vec::with_capacity(slot_count);

        // Spawn workers
        for worker_id in 0..slot_count {
            let slot = topology.slots[worker_id].clone();
            let initial_sandbox = if worker_id == 0 {
                prechecked_sandbox.take()
            } else {
                None
            };
            let task_rx_clone = task_rx.clone();
            let result_tx_clone = result_tx.clone();
            let cancel_clone = cancel.clone();
            let failed_subtasks_clone = Arc::clone(&failed_subtasks);
            let event_tx_clone = event_sender.clone();
            let limits_clone = limits.clone();
            let exe_path = compiled.binary_path.clone();
            let interpreter_cmd = compiled.interpreter_cmd.clone();
            let prepared_exe_clone = prepared_exe.clone();
            let sandbox_mode = self.options.sandbox_mode.clone();
            let require_tier = self.options.require_tier;
            let fail_fast = self.options.fail_fast;
            let checker_clone = resolved_checker.clone();
            let problem_type = problem_config.problem_type;
            let num_processes = problem_config.processes.unwrap_or(1);
            let comm_topology = problem_config.communication_topology.clone();
            let interactor_bin_clone = interactor_bin.clone();
            let manager_bin_clone = manager_bin.clone();
            let io_config = problem_config.io.clone();
            let base_dir_for_worker = base_dir_path.clone();

            let handle = thread::spawn(move || {
                let slot_box =
                    uj_sandbox::create_slot_box_dir(&base_dir_for_worker, worker_id).ok();

                let sandbox_res = match initial_sandbox {
                    Some(sb) => Ok(sb),
                    None => create_sandbox(&sandbox_mode, require_tier),
                };

                let (sandbox, init_error) = match sandbox_res {
                    Ok(sb) => (Some(sb), None),
                    Err(e) => (None, Some(e)),
                };

                let auth_error = if let Some(ref sb) = sandbox {
                    if let Some(ref p) = prepared_exe_clone {
                        sb.authorize_executable(p).err()
                    } else {
                        None
                    }
                } else {
                    None
                };

                while let Ok(test) = task_rx_clone.recv() {
                    if cancel_clone.is_cancelled() {
                        break;
                    }

                    if let Some(ref err) = init_error {
                        let failed_result = SingleTestResult {
                            id: test.name.clone(),
                            subtask: test.subtask_id,
                            verdict: Verdict::JudgeFailed,
                            score: 0.0,
                            time_ms: 0,
                            wall_ms: 0,
                            memory_kb: 0,
                            exit: Default::default(),
                            message: Some(format!("Worker failed to initialize sandbox: {err}")),
                            checker: None,
                            diff: None,
                            timing: Default::default(),
                            sandbox_tier: None,
                        };
                        let _ = result_tx_clone.send(failed_result);
                        continue;
                    }

                    let sandbox = sandbox.as_ref().unwrap();

                    if let Some(ref err) = auth_error {
                        let failed_result = SingleTestResult {
                            id: test.name.clone(),
                            subtask: test.subtask_id,
                            verdict: Verdict::JudgeFailed,
                            score: 0.0,
                            time_ms: 0,
                            wall_ms: 0,
                            memory_kb: 0,
                            exit: Default::default(),
                            message: Some(format!(
                                "Failed to authorize executable for sandbox slot: {err}"
                            )),
                            checker: None,
                            diff: None,
                            timing: Default::default(),
                            sandbox_tier: Some(sandbox.tier()),
                        };
                        let _ = result_tx_clone.send(failed_result);
                        continue;
                    }

                    // Fail fast check: skip if same subtask already failed
                    if fail_fast
                        && let Some(st_id) = test.subtask_id
                        && failed_subtasks_clone.lock().contains(&st_id)
                    {
                        let skipped_result = SingleTestResult {
                            id: test.name.clone(),
                            subtask: test.subtask_id,
                            verdict: Verdict::Skipped,
                            score: 0.0,
                            time_ms: 0,
                            wall_ms: 0,
                            memory_kb: 0,
                            exit: Default::default(),
                            message: Some("Skipped due to --fail-fast".to_string()),
                            checker: None,
                            diff: None,
                            timing: Default::default(),
                            sandbox_tier: Some(sandbox.tier()),
                        };
                        let _ = result_tx_clone.send(skipped_result);
                        continue;
                    }

                    if let Some(ref tx) = event_tx_clone {
                        let _ = tx.send(JudgeEvent::TestStart {
                            id: test.name.clone(),
                            subtask: test.subtask_id,
                        });
                    }

                    let custom_box_ref = slot_box.as_ref().map(|sb| sb.box_dir.as_path());

                    let res = match problem_type {
                        ProblemType::Interactive => {
                            let int_exe = interactor_bin_clone
                                .as_ref()
                                .expect("interactor binary must exist");
                            judge_interactive_test(
                                sandbox.as_ref(),
                                &exe_path,
                                interpreter_cmd.as_deref(),
                                prepared_exe_clone.as_deref(),
                                int_exe,
                                &test,
                                &limits_clone,
                                Some(&checker_clone),
                                pe_as_wa,
                                &cancel_clone,
                                custom_box_ref,
                            )
                        }
                        ProblemType::OutputOnly => judge_output_only_test(
                            sandbox.as_ref(),
                            &exe_path,
                            &test,
                            &checker_clone,
                            pe_as_wa,
                            &cancel_clone,
                        ),
                        ProblemType::Communication => {
                            if let Some(ref mgr_exe) = manager_bin_clone {
                                judge_communication_test(
                                    sandbox.as_ref(),
                                    mgr_exe,
                                    &exe_path,
                                    interpreter_cmd.as_deref(),
                                    prepared_exe_clone.as_deref(),
                                    &test,
                                    &limits_clone,
                                    &cancel_clone,
                                    custom_box_ref,
                                    num_processes,
                                    comm_topology.as_deref(),
                                )
                            } else {
                                judge_single_test(
                                    sandbox.as_ref(),
                                    &exe_path,
                                    interpreter_cmd.as_deref(),
                                    prepared_exe_clone.as_deref(),
                                    &test,
                                    &limits_clone,
                                    &checker_clone,
                                    pe_as_wa,
                                    slot.cpu_core,
                                    Some(&io_config),
                                    &cancel_clone,
                                    custom_box_ref,
                                )
                            }
                        }
                        ProblemType::Traditional | ProblemType::Grader => judge_single_test(
                            sandbox.as_ref(),
                            &exe_path,
                            interpreter_cmd.as_deref(),
                            prepared_exe_clone.as_deref(),
                            &test,
                            &limits_clone,
                            &checker_clone,
                            pe_as_wa,
                            slot.cpu_core,
                            Some(&io_config),
                            &cancel_clone,
                            custom_box_ref,
                        ),
                    };

                    if fail_fast
                        && !res.verdict.is_accepted()
                        && let Some(st_id) = test.subtask_id
                    {
                        failed_subtasks_clone.lock().insert(st_id);
                    }

                    if let Some(ref tx) = event_tx_clone {
                        let _ = tx.send(JudgeEvent::TestDone(res.clone()));
                    }

                    let _ = result_tx_clone.send(res);
                }
            });

            worker_handles.push(handle);
        }

        drop(result_tx); // Close original sender so result_rx terminates

        // Push tasks
        for idx in sorted_indices {
            let _ = task_tx.send(tests[idx].clone());
        }
        drop(task_tx); // Close task sender so workers complete

        // Collect results
        let mut test_results = Vec::with_capacity(total_tests);
        for res in result_rx {
            test_results.push(res);
        }

        // Wait for workers
        for h in worker_handles {
            let _ = h.join();
        }

        if test_results.is_empty() && total_tests > 0 {
            if let Some(ref tx) = event_sender {
                let _ = tx.send(JudgeEvent::Summary {
                    total_time_ms: 0,
                    peak_memory_kb: 0,
                    final_verdict: Verdict::JudgeFailed,
                    final_score: 0.0,
                });
            }
            return Err(SchedulerError::Sandbox(
                "No test results collected: all sandbox executions failed".to_string(),
            ));
        }

        // Sort results by natural test ID order
        test_results.sort_by(|a, b| uj_core::tests::natural_cmp(&a.id, &b.id));

        // --------------------------------------------------------------------
        // Section 6.3 Precise Rejudging for Edge Cases (--precise)
        // --------------------------------------------------------------------
        if self.options.precise
            && !cancel.is_cancelled()
            && problem_config.problem_type != ProblemType::OutputOnly
        {
            let tl_ms = limits.cpu_time_ms;
            let lower_bound = (tl_ms as f64 * 0.9) as u64;
            let upper_bound = (tl_ms as f64 * 1.5) as u64;

            let mut candidate_indices = Vec::new();
            for (idx, tr) in test_results.iter().enumerate() {
                let is_borderline = tr.time_ms >= lower_bound && tr.time_ms <= upper_bound;
                let is_borderline_tle =
                    tr.verdict == Verdict::TimeLimitExceeded && tr.time_ms < upper_bound;
                if is_borderline || is_borderline_tle {
                    candidate_indices.push(idx);
                }
            }

            // If there are multiple candidates, prioritize the ones closest to TL and cap at 3
            if candidate_indices.len() > 3 {
                candidate_indices.sort_by_key(|&idx| {
                    let t = test_results[idx].time_ms;
                    if t >= tl_ms {
                        t - tl_ms
                    } else {
                        tl_ms - t
                    }
                });
                candidate_indices.truncate(3);
            }

            if !candidate_indices.is_empty() {
                let exclusive_sandbox = match create_sandbox(
                    &self.options.sandbox_mode,
                    self.options.require_tier,
                ) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        for &idx in &candidate_indices {
                            test_results[idx].verdict = Verdict::JudgeFailed;
                            test_results[idx].message = Some(format!(
                                "Precise rejudge failed: could not create exclusive sandbox: {e}"
                            ));
                        }
                        None
                    }
                };

                let authorized = if let Some(ref exclusive_sandbox) = exclusive_sandbox {
                    if let Some(ref p) = prepared_exe
                        && let Err(e) = exclusive_sandbox.authorize_executable(p)
                    {
                        for &idx in &candidate_indices {
                            test_results[idx].verdict = Verdict::JudgeFailed;
                            test_results[idx].message = Some(format!(
                                "Precise rejudge failed: could not authorize executable: {e}"
                            ));
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    false
                };

                if authorized && let Some(ref exclusive_sandbox) = exclusive_sandbox {
                    let rejudge_slot_box =
                        uj_sandbox::create_slot_box_dir(&base_dir_path, 9999).ok();
                    let rejudge_box = rejudge_slot_box.as_ref().map(|sb| sb.box_dir.as_path());

                    for &idx in &candidate_indices {
                        if cancel.is_cancelled() {
                            break;
                        }

                        let target_test_id = test_results[idx].id.clone();
                        let Some(orig_test) = tests.iter().find(|t| t.name == target_test_id)
                        else {
                            continue;
                        };

                        if let Some(ref tx) = event_sender {
                            let _ = tx.send(JudgeEvent::TestStart {
                                id: target_test_id.clone(),
                                subtask: None,
                            });
                            let _ = tx.send(JudgeEvent::Warning {
                                message: format!("Precise rejudging test {} in exclusive slot...", target_test_id),
                                context: Some(target_test_id.clone()),
                            });
                        }

                        let mut samples = vec![test_results[idx].time_ms];
                        let mut best_result = test_results[idx].clone();

                        // Rejudge up to 2 times in exclusive mode
                        for _ in 0..2 {
                            if cancel.is_cancelled() {
                                break;
                            }
                            let rerun = match problem_config.problem_type {
                                ProblemType::Interactive => {
                                    let int_exe = interactor_bin
                                        .as_ref()
                                        .expect("interactor binary must exist");
                                    judge_interactive_test(
                                        exclusive_sandbox.as_ref(),
                                        &compiled.binary_path,
                                        compiled.interpreter_cmd.as_deref(),
                                        prepared_exe.as_deref(),
                                        int_exe,
                                        orig_test,
                                        &limits,
                                        Some(&resolved_checker),
                                        pe_as_wa,
                                        cancel,
                                        rejudge_box,
                                    )
                                }
                                ProblemType::Communication => {
                                    let mgr_exe = manager_bin.as_ref().expect(
                                        "manager binary must exist for communication problem",
                                    );
                                    judge_communication_test(
                                        exclusive_sandbox.as_ref(),
                                        mgr_exe,
                                        &compiled.binary_path,
                                        compiled.interpreter_cmd.as_deref(),
                                        prepared_exe.as_deref(),
                                        orig_test,
                                        &limits,
                                        cancel,
                                        rejudge_box,
                                        problem_config.processes.unwrap_or(1),
                                        problem_config.communication_topology.as_deref(),
                                    )
                                }
                                ProblemType::OutputOnly => unreachable!(),
                                _ => judge_single_test(
                                    exclusive_sandbox.as_ref(),
                                    &compiled.binary_path,
                                    compiled.interpreter_cmd.as_deref(),
                                    prepared_exe.as_deref(),
                                    orig_test,
                                    &limits,
                                    &resolved_checker,
                                    pe_as_wa,
                                    topology.slots.first().and_then(|s| s.cpu_core),
                                    Some(&problem_config.io),
                                    cancel,
                                    rejudge_box,
                                ),
                            };
                            samples.push(rerun.time_ms);
                            if rerun.time_ms < best_result.time_ms {
                                best_result = rerun;
                            }
                        }

                        let min_sample = samples.iter().copied().min().unwrap_or(1).max(1);
                        let max_sample = samples.iter().copied().max().unwrap_or(1);
                        let variance = (max_sample - min_sample) as f64 / min_sample as f64;

                        let confidence = if variance > 0.10 {
                            "low".to_string()
                        } else {
                            "medium".to_string()
                        };

                        best_result.timing.rejudged = true;
                        best_result.timing.confidence = confidence;
                        test_results[idx] = best_result.clone();

                        if let Some(ref tx) = event_sender {
                            let _ = tx.send(JudgeEvent::TestDone(best_result));
                        }
                    }
                }
            }
        }

        let report = aggregate_submission_report(
            &problem_config.name,
            submission_name,
            problem_config,
            test_results,
        );

        if let Some(ref tx) = event_sender {
            for st in &report.subtasks {
                let _ = tx.send(JudgeEvent::SubtaskDone(st.clone()));
            }
            let _ = tx.send(JudgeEvent::SubmissionDone(report.clone()));
            let _ = tx.send(JudgeEvent::Summary {
                total_time_ms: report.max_time_ms,
                peak_memory_kb: report.max_memory_kb,
                final_verdict: report.verdict,
                final_score: report.score,
            });
        }

        Ok(report)
    }
}

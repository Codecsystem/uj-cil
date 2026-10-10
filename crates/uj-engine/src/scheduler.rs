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

use crate::compiler::{CompileResult, Compiler};
use crate::judge::{
    JudgeOptions, ResolvedChecker, aggregate_submission_report, judge_communication_test,
    judge_interactive_test, judge_output_only_test, judge_single_test,
};
use crate::topology::{Slot, Topology};

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
}

impl Scheduler {
    pub fn new(options: JudgeOptions) -> Self {
        Self { options }
    }

    /// Run full judging pipeline for a submission against a problem.
    #[allow(clippy::too_many_arguments)]
    pub fn judge_submission(
        &self,
        problem_dir: &Path,
        problem_config: &ProblemConfig,
        compiled: &CompileResult,
        tests: Vec<TestCase>,
        user_jobs: Option<usize>,
        event_sender: Option<Sender<JudgeEvent>>,
        cancel: &CancelToken,
    ) -> Result<SubmissionJudgeReport, SchedulerError> {
        let limits = problem_config
            .to_limits()
            .map_err(|e| SchedulerError::Config(e.to_string()))?;
        let topology = Topology::detect(limits.memory_bytes, user_jobs);

        #[cfg(windows)]
        {
            let _ =
                uj_sandbox::windows::security::AppContainerProfile::grant_all_application_packages(
                    problem_dir,
                );
        }

        let pe_as_wa = problem_config.checker.pe_as_wa;

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
                    let compiled_chk = Compiler::compile(&chk_path, false).map_err(|e| {
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
                    let compiled_chk = Compiler::compile(&chk_path, false).map_err(|e| {
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
                let compiled_int = Compiler::compile(&int_path, false).map_err(|e| {
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
                let compiled_mgr = Compiler::compile(&mgr_path, false).map_err(|e| {
                    SchedulerError::Config(format!("Failed to compile manager: {e}"))
                })?;
                compiled_mgr.binary_path
            };
            Some(bin_path)
        } else {
            None
        };

        // Sort tests by input file size descending (LPT heuristic)
        let mut sorted_tests = tests.clone();
        sorted_tests.sort_by(|a, b| {
            let size_a = fs::metadata(&a.input_path).map(|m| m.len()).unwrap_or(0);
            let size_b = fs::metadata(&b.input_path).map(|m| m.len()).unwrap_or(0);
            size_b.cmp(&size_a)
        });

        let total_tests = sorted_tests.len();
        if let Some(ref tx) = event_sender {
            let _ = tx.send(JudgeEvent::Summary {
                total_time_ms: 0,
                peak_memory_kb: 0,
                final_verdict: Verdict::Accepted,
                final_score: 0.0,
            });
        }

        // Shared state for fail-fast subtask tracking
        let failed_subtasks: Arc<Mutex<std::collections::HashSet<u32>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));

        let (task_tx, task_rx) = unbounded::<(Slot, TestCase)>();
        let (result_tx, result_rx) = bounded::<SingleTestResult>(total_tests.max(1));

        let slot_count = topology.slots.len();
        let mut worker_handles = Vec::with_capacity(slot_count);

        // Spawn workers
        for _ in 0..slot_count {
            let task_rx_clone = task_rx.clone();
            let result_tx_clone = result_tx.clone();
            let cancel_clone = cancel.clone();
            let failed_subtasks_clone = Arc::clone(&failed_subtasks);
            let event_tx_clone = event_sender.clone();
            let limits_clone = limits.clone();
            let exe_path = compiled.binary_path.clone();
            let interpreter_cmd = compiled.interpreter_cmd.clone();
            let sandbox_mode = self.options.sandbox_mode.clone();
            let require_tier = self.options.require_tier;
            let fail_fast = self.options.fail_fast;
            let checker_clone = resolved_checker.clone();
            let problem_type = problem_config.problem_type;
            let interactor_bin_clone = interactor_bin.clone();
            let manager_bin_clone = manager_bin.clone();

            let handle = thread::spawn(move || {
                let sandbox = match create_sandbox(&sandbox_mode, require_tier) {
                    Ok(sb) => sb,
                    Err(_) => return,
                };

                while let Ok((s, test)) = task_rx_clone.recv() {
                    if cancel_clone.is_cancelled() {
                        break;
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
                            checker: Some("Skipped due to --fail-fast".to_string()),
                            diff: None,
                            timing: Default::default(),
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

                    let res = match problem_type {
                        ProblemType::Interactive => {
                            let int_exe = interactor_bin_clone
                                .as_ref()
                                .expect("interactor binary must exist");
                            judge_interactive_test(
                                sandbox.as_ref(),
                                &exe_path,
                                interpreter_cmd.as_deref(),
                                int_exe,
                                &test,
                                &limits_clone,
                                Some(&checker_clone),
                                pe_as_wa,
                                &cancel_clone,
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
                                    &test,
                                    &limits_clone,
                                    &cancel_clone,
                                )
                            } else {
                                judge_single_test(
                                    sandbox.as_ref(),
                                    &exe_path,
                                    interpreter_cmd.as_deref(),
                                    &test,
                                    &limits_clone,
                                    &checker_clone,
                                    pe_as_wa,
                                    s.cpu_core,
                                    &cancel_clone,
                                )
                            }
                        }
                        ProblemType::Traditional | ProblemType::Grader => judge_single_test(
                            sandbox.as_ref(),
                            &exe_path,
                            interpreter_cmd.as_deref(),
                            &test,
                            &limits_clone,
                            &checker_clone,
                            pe_as_wa,
                            s.cpu_core,
                            &cancel_clone,
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
        let slots = &topology.slots;
        for (i, test) in sorted_tests.into_iter().enumerate() {
            let slot = slots[i % slot_count].clone();
            let _ = task_tx.send((slot, test));
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

            if !candidate_indices.is_empty()
                && let Ok(exclusive_sandbox) =
                    create_sandbox(&self.options.sandbox_mode, self.options.require_tier)
            {
                for &idx in &candidate_indices {
                    if cancel.is_cancelled() {
                        break;
                    }

                    let target_test_id = test_results[idx].id.clone();
                    let Some(orig_test) = tests.iter().find(|t| t.name == target_test_id) else {
                        continue;
                    };

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
                                    int_exe,
                                    orig_test,
                                    &limits,
                                    Some(&resolved_checker),
                                    pe_as_wa,
                                    cancel,
                                )
                            }
                            ProblemType::OutputOnly => unreachable!(),
                            _ => judge_single_test(
                                exclusive_sandbox.as_ref(),
                                &compiled.binary_path,
                                compiled.interpreter_cmd.as_deref(),
                                orig_test,
                                &limits,
                                &resolved_checker,
                                pe_as_wa,
                                Some(1),
                                cancel,
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
                    best_result.timing.samples_ms = samples;

                    test_results[idx] = best_result;
                }
            }
        }

        let report = aggregate_submission_report(
            &problem_config.name,
            &compiled
                .binary_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            problem_config,
            test_results,
        );

        if let Some(ref tx) = event_sender {
            let _ = tx.send(JudgeEvent::SubmissionDone(report.clone()));
        }

        Ok(report)
    }
}

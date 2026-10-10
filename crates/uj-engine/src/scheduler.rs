//! Multi-threaded test case scheduler, LPT heuristic queue, and event dispatch.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;

use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::ProblemConfig;
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{JudgeEvent, SingleTestResult, SubmissionJudgeReport};
use uj_sandbox::{CancelToken, create_sandbox};

use crate::compiler::CompileResult;
use crate::judge::{JudgeOptions, aggregate_submission_report, judge_single_test};
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
        _problem_dir: &Path,
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

        let checker_name = problem_config.checker.name.as_deref().unwrap_or("wcmp");
        let checker_type =
            BuiltinCheckerType::parse(checker_name).unwrap_or(BuiltinCheckerType::Wcmp);
        let pe_as_wa = problem_config.checker.pe_as_wa;

        // Sort tests by input file size descending (LPT heuristic)
        let mut sorted_tests = tests;
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

                    let res = judge_single_test(
                        sandbox.as_ref(),
                        &exe_path,
                        interpreter_cmd.as_deref(),
                        &test,
                        &limits_clone,
                        checker_type,
                        pe_as_wa,
                        s.cpu_core,
                        &cancel_clone,
                    );

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

//! Global judging pool with memory budget and concurrent submission evaluation.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Sender, bounded, unbounded};
use uj_core::config::ProblemType;
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{
    JudgeEvent, PoolEvent, SingleTestResult, SubmissionJudgeReport, SubmissionRef,
};
use uj_sandbox::{CancelToken, SlotBoxGuard, create_sandbox};

use crate::budget::{MemoryBudget, task_memory_cost};
use crate::context::{SubmissionContext, run_one_test};
use crate::judge::aggregate_submission_report;
use crate::scheduler::SchedulerError;
use crate::topology::Topology;

/// Submission submitted to the global judging pool.
pub struct PoolSubmission {
    pub ctx: Arc<SubmissionContext>,
    /// Event stream for this submission (progress bar / single TUI / NDJSON)
    pub events: Option<Sender<JudgeEvent>>,
    pub precise: bool,
}

/// Configuration options for the global judging pool.
#[derive(Debug, Clone)]
pub struct PoolOptions {
    pub sandbox_mode: String,
    pub require_tier: bool,
    pub slots: usize,
    pub window: usize,
    pub memory_budget: u64,
    pub pool_events: Option<Sender<PoolEvent>>,
}

impl PoolOptions {
    pub fn default_for(sandbox_mode: String, require_tier: bool, user_jobs: Option<usize>) -> Self {
        let topology = Topology::detect(0, user_jobs);
        let slots = topology.slots.len().max(1);
        let window = (slots / 2).max(2);
        let memory_budget = Topology::memory_budget_bytes();
        Self {
            sandbox_mode,
            require_tier,
            slots,
            window,
            memory_budget,
            pool_events: None,
        }
    }
}

pub struct JudgePool {
    opts: PoolOptions,
}

struct PoolTask {
    sub: Arc<SubmissionContext>,
    test: TestCase,
    event_tx: Option<Sender<JudgeEvent>>,
}

struct TaskResult {
    sub_id: usize,
    result: SingleTestResult,
}

struct PreparedSub {
    sub: Arc<SubmissionContext>,
    sorted_tests: Vec<TestCase>,
    event_tx: Option<Sender<JudgeEvent>>,
    precise: bool,
}

impl JudgePool {
    pub fn new(opts: PoolOptions) -> Self {
        Self { opts }
    }

    /// Run evaluations for all submissions concurrently across worker slots.
    /// Results are returned in the exact order of the input `subs` slice.
    pub fn run(
        &self,
        subs: Vec<PoolSubmission>,
        cancel: &CancelToken,
    ) -> Vec<Result<SubmissionJudgeReport, SchedulerError>> {
        if subs.is_empty() {
            return Vec::new();
        }

        // Pre-validate sandbox initialization and required tier on slot 0
        let prechecked_sandbox =
            match create_sandbox(&self.opts.sandbox_mode, self.opts.require_tier) {
                Ok(sb) => Some(sb),
                Err(e) => {
                    let msg = format!("Failed to initialize sandbox: {e}");
                    return subs
                        .iter()
                        .map(|_| Err(SchedulerError::Sandbox(msg.clone())))
                        .collect();
                }
            };

        // Warn once if any problem has memory limit == 0
        for sub in &subs {
            if sub.ctx.problem.limits.memory_bytes == 0 {
                static WARNED_NO_MEM_LIMIT: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED_NO_MEM_LIMIT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "Notice: problem has memory limit set to 0 (unlimited memory); memory budget constraint disabled for this problem."
                    );
                }
                break;
            }
        }

        let topology = Topology::detect(0, Some(self.opts.slots));
        let worker_count = self.opts.slots.max(1);
        let memory_budget = Arc::new(MemoryBudget::new(self.opts.memory_budget));

        // Prepare tests in LPT order for each submission
        let mut expected_tests_count = HashMap::with_capacity(subs.len());
        let mut prepared_subs = Vec::with_capacity(subs.len());
        let mut total_all_tests = 0;

        for (idx, pool_sub) in subs.into_iter().enumerate() {
            let sub_id = pool_sub.ctx.submission_id;
            let mut sorted_tests = pool_sub.ctx.tests.clone();
            if pool_sub.ctx.problem.has_depends {
                let ranks = &pool_sub.ctx.problem.subtask_topo_rank;
                sorted_tests.sort_by_cached_key(|t| {
                    let topo_rank = t
                        .subtask_id
                        .and_then(|id| ranks.get(&id).copied())
                        .unwrap_or(0);
                    (
                        topo_rank,
                        Reverse(uj_core::tests::get_input_file_size(&t.input_path)),
                    )
                });
            } else {
                sorted_tests.sort_by_cached_key(|t| {
                    Reverse(uj_core::tests::get_input_file_size(&t.input_path))
                });
            }
            let count = sorted_tests.len();
            total_all_tests += count;
            expected_tests_count.insert(sub_id, count);
            prepared_subs.push(PreparedSub {
                sub: pool_sub.ctx,
                sorted_tests,
                event_tx: pool_sub.events,
                precise: pool_sub.precise,
            });
            let _ = idx;
        }

        let (task_tx, task_rx) = unbounded::<PoolTask>();
        let (result_tx, result_rx) = bounded::<TaskResult>(total_all_tests.max(1));
        let (done_tx, done_rx) = unbounded::<usize>();

        // Spawn worker pool
        let mut worker_handles = Vec::with_capacity(worker_count);
        let mut prechecked_opt = prechecked_sandbox;

        for worker_id in 0..worker_count {
            let slot = topology.slots[worker_id % topology.slots.len()].clone();
            let initial_sandbox = if worker_id == 0 {
                prechecked_opt.take()
            } else {
                None
            };
            let task_rx_clone = task_rx.clone();
            let result_tx_clone = result_tx.clone();
            let cancel_clone = cancel.clone();
            let budget_clone = Arc::clone(&memory_budget);
            let sandbox_mode = self.opts.sandbox_mode.clone();
            let require_tier = self.opts.require_tier;
            let pool_events_clone = self.opts.pool_events.clone();

            let handle = thread::spawn(move || {
                let slot_id = slot.id;
                let cpu_core = slot.cpu_core;

                let sandbox_res = match initial_sandbox {
                    Some(sb) => Ok(sb),
                    None => create_sandbox(&sandbox_mode, require_tier),
                };
                let (sandbox, init_error) = match sandbox_res {
                    Ok(sb) => (Some(sb), None),
                    Err(e) => (None, Some(e)),
                };

                let mut boxes: HashMap<usize, SlotBoxGuard> = HashMap::new();
                let mut auth_failed: HashMap<PathBuf, String> = HashMap::new();

                while let Ok(task) = task_rx_clone.recv() {
                    if cancel_clone.is_cancelled() {
                        break;
                    }
                    let sub = task.sub;
                    let test = task.test;
                    let event_tx = task.event_tx;

                    if let Some(ref err) = init_error {
                        if let Some(st_id) = test.subtask_id {
                            sub.complete_subtask_test(st_id, false);
                        }
                        let failed_res = SingleTestResult::synthetic(
                            &test,
                            Verdict::JudgeFailed,
                            Some(format!("Worker failed to initialize sandbox: {err}")),
                            None,
                        );
                        let _ = result_tx_clone.send(TaskResult {
                            sub_id: sub.submission_id,
                            result: failed_res,
                        });
                        continue;
                    }

                    let sb = sandbox.as_ref().unwrap();

                    // Check subtask DAG dependencies (VULN-06: always active when problem has depends)
                    if sub.problem.has_depends
                        && let Some(st_id) = test.subtask_id
                        && let Some(failed_dep) = sub.wait_for_dependencies(st_id, &cancel_clone)
                    {
                        sub.complete_subtask_test(st_id, false);
                        let skipped_res = SingleTestResult::synthetic(
                            &test,
                            Verdict::Skipped,
                            Some(format!(
                                "Skipped due to failed dependency subtask {failed_dep}"
                            )),
                            Some(sb.tier()),
                        );
                        let _ = result_tx_clone.send(TaskResult {
                            sub_id: sub.submission_id,
                            result: skipped_res,
                        });
                        continue;
                    }

                    // Check fail-fast within same subtask
                    if sub.fail_fast
                        && let Some(st_id) = test.subtask_id
                        && sub.is_subtask_failed(st_id)
                    {
                        sub.complete_subtask_test(st_id, false);
                        let skipped_res = SingleTestResult::synthetic(
                            &test,
                            Verdict::Skipped,
                            Some("Skipped due to --fail-fast".to_string()),
                            Some(sb.tier()),
                        );
                        let _ = result_tx_clone.send(TaskResult {
                            sub_id: sub.submission_id,
                            result: skipped_res,
                        });
                        continue;
                    }

                    // Authorize executable
                    if let Some(ref p) = sub.prepared_exe {
                        if let Some(prev_err) = auth_failed.get(p) {
                            if let Some(st_id) = test.subtask_id {
                                sub.complete_subtask_test(st_id, false);
                            }
                            let failed_res = SingleTestResult::synthetic(
                                &test,
                                Verdict::JudgeFailed,
                                Some(format!(
                                    "Failed to authorize executable for sandbox slot: {prev_err}"
                                )),
                                Some(sb.tier()),
                            );
                            let _ = result_tx_clone.send(TaskResult {
                                sub_id: sub.submission_id,
                                result: failed_res,
                            });
                            continue;
                        }
                        if let Err(e) = sb.authorize_executable(p) {
                            if let Some(st_id) = test.subtask_id {
                                sub.complete_subtask_test(st_id, false);
                            }
                            auth_failed.insert(p.clone(), e.to_string());
                            let failed_res = SingleTestResult::synthetic(
                                &test,
                                Verdict::JudgeFailed,
                                Some(format!(
                                    "Failed to authorize executable for sandbox slot: {e}"
                                )),
                                Some(sb.tier()),
                            );
                            let _ = result_tx_clone.send(TaskResult {
                                sub_id: sub.submission_id,
                                result: failed_res,
                            });
                            continue;
                        }
                    }

                    // Acquire memory budget
                    let mem_cost = task_memory_cost(&sub.problem.config, &sub.problem.limits);
                    let lease = match budget_clone.acquire(mem_cost, &cancel_clone) {
                        Some(l) => l,
                        None => {
                            break;
                        }
                    };

                    let start_ev = JudgeEvent::TestStart {
                        id: test.name.clone(),
                        subtask: test.subtask_id,
                        slot: Some(slot_id),
                    };
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(start_ev.clone());
                    }
                    if let Some(ref ptx) = pool_events_clone {
                        let _ = ptx.send(PoolEvent {
                            submission: SubmissionRef {
                                id: sub.submission_id,
                                problem: sub.problem.config.name.clone(),
                                name: sub.submission_name.clone(),
                            },
                            inner: start_ev,
                        });
                    }

                    // Lazy create slot box per problem
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        boxes.entry(sub.problem.problem_id)
                        && let Ok(box_guard) =
                            uj_sandbox::create_slot_box_dir(&sub.base_dir_path, slot_id)
                    {
                        e.insert(box_guard);
                    }
                    let custom_box_ref = boxes
                        .get(&sub.problem.problem_id)
                        .map(|b| b.box_dir.as_path());

                    let res = run_one_test(
                        sb.as_ref(),
                        &sub,
                        &test,
                        cpu_core,
                        custom_box_ref,
                        &cancel_clone,
                    );

                    drop(lease);

                    if let Some(st_id) = test.subtask_id
                        && (sub.problem.has_depends || sub.fail_fast)
                    {
                        sub.complete_subtask_test(st_id, res.verdict.is_accepted());
                    }

                    let done_ev = JudgeEvent::TestDone(res.clone());
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(done_ev.clone());
                    }
                    if let Some(ref ptx) = pool_events_clone {
                        let _ = ptx.send(PoolEvent {
                            submission: SubmissionRef {
                                id: sub.submission_id,
                                problem: sub.problem.config.name.clone(),
                                name: sub.submission_name.clone(),
                            },
                            inner: done_ev,
                        });
                    }

                    let _ = result_tx_clone.send(TaskResult {
                        sub_id: sub.submission_id,
                        result: res,
                    });
                }
            });

            worker_handles.push(handle);
        }

        let prepared_subs = Arc::new(prepared_subs);
        let prepared_subs_adm = Arc::clone(&prepared_subs);

        // Spawn admission thread
        let window = self.opts.window.max(1);
        let cancel_admission = cancel.clone();
        let mut opt_task_tx = Some(task_tx);

        let admission_handle = thread::spawn(move || {
            let mut next_sub_idx = 0;
            let mut inflight = 0;

            while next_sub_idx < prepared_subs_adm.len() || inflight > 0 {
                if cancel_admission.is_cancelled() {
                    opt_task_tx.take();
                    break;
                }

                while inflight < window && next_sub_idx < prepared_subs_adm.len() {
                    let p_sub = &prepared_subs_adm[next_sub_idx];
                    if let Some(ref tx) = opt_task_tx {
                        for test in &p_sub.sorted_tests {
                            let _ = tx.send(PoolTask {
                                sub: Arc::clone(&p_sub.sub),
                                test: test.clone(),
                                event_tx: p_sub.event_tx.clone(),
                            });
                        }
                    }
                    inflight += 1;
                    next_sub_idx += 1;
                }

                if next_sub_idx >= prepared_subs_adm.len() {
                    opt_task_tx.take();
                }

                if inflight == 0 {
                    break;
                }

                // Wait for a submission to complete
                match done_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(_sub_id) => {
                        inflight = inflight.saturating_sub(1);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Check cancellation and loop
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
        });

        // Drop original result_tx so result_rx terminates when all workers finish
        drop(result_tx);

        // Collector loop (main thread)
        let mut results_by_sub: HashMap<usize, Vec<SingleTestResult>> = HashMap::new();
        let mut completed_sub_ids = std::collections::HashSet::new();
        let mut deferred_precise = Vec::new();
        let mut final_reports: HashMap<usize, Result<SubmissionJudgeReport, SchedulerError>> =
            HashMap::new();

        // Check for 0-test submissions immediately
        for (&sub_id, &count) in &expected_tests_count {
            if count == 0 {
                completed_sub_ids.insert(sub_id);
                let _ = done_tx.send(sub_id);
            }
        }

        while let Ok(task_res) = result_rx.recv() {
            let sub_id = task_res.sub_id;
            let list = results_by_sub.entry(sub_id).or_default();
            list.push(task_res.result);

            let exp_count = *expected_tests_count.get(&sub_id).unwrap_or(&0);
            if list.len() >= exp_count && !completed_sub_ids.contains(&sub_id) {
                completed_sub_ids.insert(sub_id);
                let _ = done_tx.send(sub_id);

                if let Some(p_sub) = prepared_subs.iter().find(|p| p.sub.submission_id == sub_id)
                    && (!p_sub.precise
                        || p_sub.sub.problem.config.problem_type == ProblemType::OutputOnly)
                {
                    let mut test_results = results_by_sub.remove(&sub_id).unwrap_or_default();
                    test_results.sort_by(|a, b| uj_core::tests::natural_cmp(&a.id, &b.id));
                    let report = aggregate_and_dispatch(
                        &p_sub.sub,
                        test_results,
                        p_sub.event_tx.as_ref(),
                        self.opts.pool_events.as_ref(),
                    );
                    final_reports.insert(sub_id, Ok(report));
                }
            }
        }

        // Wait for admission thread and workers
        let _ = admission_handle.join();

        for h in worker_handles {
            let _ = h.join();
        }

        // Process completed results or categorize into precise / non-precise
        for p_sub in prepared_subs.iter() {
            let sub_id = p_sub.sub.submission_id;
            if final_reports.contains_key(&sub_id) {
                continue;
            }
            let mut test_results = results_by_sub.remove(&sub_id).unwrap_or_default();
            let exp_count = *expected_tests_count.get(&sub_id).unwrap_or(&0);

            let all_failed_init = !test_results.is_empty()
                && test_results.iter().all(|r| {
                    r.verdict == Verdict::JudgeFailed
                        && r.message
                            .as_deref()
                            .is_some_and(|m| m.starts_with("Worker failed to initialize sandbox"))
                });

            if (test_results.is_empty() && exp_count > 0) || all_failed_init {
                if let Some(ref tx) = p_sub.event_tx {
                    let _ = tx.send(JudgeEvent::Summary {
                        total_time_ms: 0,
                        peak_memory_kb: 0,
                        final_verdict: Verdict::JudgeFailed,
                        final_score: 0.0,
                    });
                }
                final_reports.insert(
                    sub_id,
                    Err(SchedulerError::Sandbox(
                        "No test results collected: all sandbox executions failed".to_string(),
                    )),
                );
                continue;
            }

            test_results.sort_by(|a, b| uj_core::tests::natural_cmp(&a.id, &b.id));

            if p_sub.precise
                && !cancel.is_cancelled()
                && p_sub.sub.problem.config.problem_type != ProblemType::OutputOnly
            {
                deferred_precise.push((p_sub, test_results));
            } else {
                let report = aggregate_and_dispatch(
                    &p_sub.sub,
                    test_results,
                    p_sub.event_tx.as_ref(),
                    self.opts.pool_events.as_ref(),
                );
                final_reports.insert(sub_id, Ok(report));
            }
        }

        // Phase C: Deferred precise rejudging in exclusive slot
        if !deferred_precise.is_empty() && !cancel.is_cancelled() {
            let exclusive_cpu = topology.slots.first().and_then(|s| s.cpu_core);

            for (p_sub, mut test_results) in deferred_precise {
                if cancel.is_cancelled() {
                    let report = aggregate_and_dispatch(
                        &p_sub.sub,
                        test_results,
                        p_sub.event_tx.as_ref(),
                        self.opts.pool_events.as_ref(),
                    );
                    final_reports.insert(p_sub.sub.submission_id, Ok(report));
                    continue;
                }

                let limits = &p_sub.sub.problem.limits;
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

                if candidate_indices.len() > 3 {
                    candidate_indices.sort_by_key(|&idx| {
                        let t = test_results[idx].time_ms;
                        t.abs_diff(tl_ms)
                    });
                    candidate_indices.truncate(3);
                }

                if !candidate_indices.is_empty() {
                    let exclusive_sandbox = match create_sandbox(
                        &self.opts.sandbox_mode,
                        self.opts.require_tier,
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
                        if let Some(ref p) = p_sub.sub.prepared_exe
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
                            uj_sandbox::create_slot_box_dir(&p_sub.sub.base_dir_path, 9999).ok();
                        let rejudge_box = rejudge_slot_box.as_ref().map(|sb| sb.box_dir.as_path());

                        for &idx in &candidate_indices {
                            if cancel.is_cancelled() {
                                break;
                            }

                            let target_test_id = test_results[idx].id.clone();
                            let Some(orig_test) =
                                p_sub.sub.tests.iter().find(|t| t.name == target_test_id)
                            else {
                                continue;
                            };

                            let start_ev = JudgeEvent::TestStart {
                                id: target_test_id.clone(),
                                subtask: None,
                                slot: Some(0),
                            };
                            let warn_ev = JudgeEvent::Warning {
                                message: format!(
                                    "Precise rejudging test {} in exclusive slot...",
                                    target_test_id
                                ),
                                context: Some(target_test_id.clone()),
                            };

                            if let Some(ref tx) = p_sub.event_tx {
                                let _ = tx.send(start_ev.clone());
                                let _ = tx.send(warn_ev.clone());
                            }
                            if let Some(ref ptx) = self.opts.pool_events {
                                let sub_ref = SubmissionRef {
                                    id: p_sub.sub.submission_id,
                                    problem: p_sub.sub.problem.config.name.clone(),
                                    name: p_sub.sub.submission_name.clone(),
                                };
                                let _ = ptx.send(PoolEvent {
                                    submission: sub_ref.clone(),
                                    inner: start_ev,
                                });
                                let _ = ptx.send(PoolEvent {
                                    submission: sub_ref,
                                    inner: warn_ev,
                                });
                            }

                            let mut samples = vec![test_results[idx].time_ms];
                            let mut best_result = test_results[idx].clone();

                            for _ in 0..2 {
                                if cancel.is_cancelled() {
                                    break;
                                }
                                let rerun = run_one_test(
                                    exclusive_sandbox.as_ref(),
                                    &p_sub.sub,
                                    orig_test,
                                    exclusive_cpu,
                                    rejudge_box,
                                    cancel,
                                );
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

                            let done_ev = JudgeEvent::TestDone(best_result);
                            if let Some(ref tx) = p_sub.event_tx {
                                let _ = tx.send(done_ev.clone());
                            }
                            if let Some(ref ptx) = self.opts.pool_events {
                                let sub_ref = SubmissionRef {
                                    id: p_sub.sub.submission_id,
                                    problem: p_sub.sub.problem.config.name.clone(),
                                    name: p_sub.sub.submission_name.clone(),
                                };
                                let _ = ptx.send(PoolEvent {
                                    submission: sub_ref,
                                    inner: done_ev,
                                });
                            }
                        }
                    }
                }

                let report = aggregate_and_dispatch(
                    &p_sub.sub,
                    test_results,
                    p_sub.event_tx.as_ref(),
                    self.opts.pool_events.as_ref(),
                );
                final_reports.insert(p_sub.sub.submission_id, Ok(report));
            }
        }

        // Return reports ordered exactly matching input prepared_subs
        prepared_subs
            .iter()
            .map(|p_sub| {
                final_reports
                    .remove(&p_sub.sub.submission_id)
                    .unwrap_or_else(|| {
                        Err(SchedulerError::Sandbox(
                            "Judge execution was interrupted before completion".to_string(),
                        ))
                    })
            })
            .collect()
    }
}

fn aggregate_and_dispatch(
    sub: &SubmissionContext,
    test_results: Vec<SingleTestResult>,
    event_tx: Option<&Sender<JudgeEvent>>,
    pool_events: Option<&Sender<PoolEvent>>,
) -> SubmissionJudgeReport {
    let report = aggregate_submission_report(
        &sub.problem.config.name,
        &sub.submission_name,
        &sub.problem.config,
        test_results,
    );

    let sub_ref = SubmissionRef {
        id: sub.submission_id,
        problem: sub.problem.config.name.clone(),
        name: sub.submission_name.clone(),
    };

    let send_ev = |ev: JudgeEvent| {
        if let Some(tx) = event_tx {
            let _ = tx.send(ev.clone());
        }
        if let Some(ptx) = pool_events {
            let _ = ptx.send(PoolEvent {
                submission: sub_ref.clone(),
                inner: ev,
            });
        }
    };

    for st in &report.subtasks {
        send_ev(JudgeEvent::SubtaskDone(st.clone()));
    }
    send_ev(JudgeEvent::SubmissionDone(report.clone()));
    send_ev(JudgeEvent::Summary {
        total_time_ms: report.max_time_ms,
        peak_memory_kb: report.max_memory_kb,
        final_verdict: report.verdict,
        final_score: report.score,
    });

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ProblemContext;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uj_core::config::ProblemConfig;

    fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
        fs::create_dir_all(&dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
            } else {
                fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    /// 将 aplusb fixture 复制到独立临时目录，避免测试之间、测试与仓库之间共享 .uj 状态。
    fn temp_aplusb() -> (tempfile::TempDir, PathBuf) {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/problems/aplusb");
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("aplusb");
        copy_dir_all(&src, &dst).unwrap();
        let _ = fs::remove_dir_all(dst.join(".uj"));
        (tmp, dst)
    }

    #[test]
    fn test_pool_results_match_serial() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let p_content = fs::read_to_string(&p_toml).unwrap();
        let cfg = ProblemConfig::from_toml_str(&p_content, Some(&p_toml)).unwrap();

        let session = crate::compiler::CompileSession::new();
        let sol = aplusb_dir.join("solutions").join("ac.cpp");
        let compiled = session.compile(&sol, false).unwrap();

        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();

        let cancel = CancelToken::new();

        // 1. Serial run via JudgePool with slots = 1
        let sub1 = Arc::new(
            SubmissionContext::prepare(
                1,
                "serial_sub",
                Arc::clone(&p_ctx),
                &compiled,
                tests.clone(),
                false,
            )
            .unwrap(),
        );

        let pool_serial = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 1,
            window: 1,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let mut res_serial = pool_serial.run(
            vec![PoolSubmission {
                ctx: sub1,
                events: None,
                precise: false,
            }],
            &cancel,
        );
        let report_serial = res_serial.pop().unwrap().unwrap();

        // 2. Parallel run via JudgePool with slots = 4
        let sub2 = Arc::new(
            SubmissionContext::prepare(
                2,
                "parallel_sub",
                Arc::clone(&p_ctx),
                &compiled,
                tests,
                false,
            )
            .unwrap(),
        );

        let pool_parallel = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 4,
            window: 2,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let mut res_parallel = pool_parallel.run(
            vec![PoolSubmission {
                ctx: sub2,
                events: None,
                precise: false,
            }],
            &cancel,
        );
        let report_parallel = res_parallel.pop().unwrap().unwrap();

        assert_eq!(report_serial.verdict, report_parallel.verdict);
        assert_eq!(report_serial.score, report_parallel.score);
        assert_eq!(report_serial.tests.len(), report_parallel.tests.len());
    }

    #[test]
    fn test_pool_fail_fast_is_per_submission() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let p_content = fs::read_to_string(&p_toml).unwrap();
        let cfg = ProblemConfig::from_toml_str(&p_content, Some(&p_toml)).unwrap();

        let session = crate::compiler::CompileSession::new();
        let sol_ac = aplusb_dir.join("solutions").join("ac.cpp");
        let sol_wa = aplusb_dir.join("solutions").join("wa.cpp");

        let comp_ac = session.compile(&sol_ac, false).unwrap();
        let comp_wa = session.compile(&sol_wa, false).unwrap();

        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();

        let cancel = CancelToken::new();

        let sub_wa = Arc::new(
            SubmissionContext::prepare(
                1,
                "wa_sub",
                Arc::clone(&p_ctx),
                &comp_wa,
                tests.clone(),
                true,
            )
            .unwrap(),
        );

        let sub_ac = Arc::new(
            SubmissionContext::prepare(2, "ac_sub", Arc::clone(&p_ctx), &comp_ac, tests, true)
                .unwrap(),
        );

        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 4,
            window: 2,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let reports = pool.run(
            vec![
                PoolSubmission {
                    ctx: sub_wa,
                    events: None,
                    precise: false,
                },
                PoolSubmission {
                    ctx: sub_ac,
                    events: None,
                    precise: false,
                },
            ],
            &cancel,
        );

        let rep_wa = reports[0].as_ref().unwrap();
        let rep_ac = reports[1].as_ref().unwrap();

        assert_ne!(rep_wa.verdict, Verdict::Accepted);
        // AC submission must NOT have its tests skipped due to WA submission's failure!
        assert_eq!(rep_ac.verdict, Verdict::Accepted);
        for t in &rep_ac.tests {
            assert_ne!(t.verdict, Verdict::Skipped);
        }
    }

    #[test]
    fn test_pool_cancel_no_orphans() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let p_content = fs::read_to_string(&p_toml).unwrap();
        let cfg = ProblemConfig::from_toml_str(&p_content, Some(&p_toml)).unwrap();

        let session = crate::compiler::CompileSession::new();
        let sol_ac = aplusb_dir.join("solutions").join("ac.cpp");
        let comp_ac = session.compile(&sol_ac, false).unwrap();

        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();

        let cancel = CancelToken::new();
        // Cancel immediately
        cancel.cancel();

        let sub_ac = Arc::new(
            SubmissionContext::prepare(1, "ac_sub", Arc::clone(&p_ctx), &comp_ac, tests, false)
                .unwrap(),
        );

        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 2,
            window: 2,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let runs_dir = aplusb_dir.join(".uj").join("tmp").join("runs");
        let initial_count = if runs_dir.exists() {
            fs::read_dir(&runs_dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().starts_with("slot-"))
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };

        let start = std::time::Instant::now();
        let _reports = pool.run(
            vec![PoolSubmission {
                ctx: sub_ac,
                events: None,
                precise: false,
            }],
            &cancel,
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "Cancel should return quickly"
        );

        // Verify no orphan slot directories under base_dir/.uj/tmp/runs were created
        let after_count = if runs_dir.exists() {
            fs::read_dir(&runs_dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().starts_with("slot-"))
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };
        assert_eq!(
            after_count, initial_count,
            "No additional leftover slot-* directories should remain"
        );
    }

    #[test]
    fn test_pool_slot_box_cross_problem_clean() {
        let (_tmp1, aplusb_dir1) = temp_aplusb();
        let (_tmp2, aplusb_dir2) = temp_aplusb();

        let p_toml1 = aplusb_dir1.join("problem.toml");
        let cfg1 =
            ProblemConfig::from_toml_str(&fs::read_to_string(&p_toml1).unwrap(), Some(&p_toml1))
                .unwrap();
        let p_toml2 = aplusb_dir2.join("problem.toml");
        let cfg2 =
            ProblemConfig::from_toml_str(&fs::read_to_string(&p_toml2).unwrap(), Some(&p_toml2))
                .unwrap();

        let session = crate::compiler::CompileSession::new();

        // Write marker test solution that checks for leftover marker files
        let marker_cpp = r#"
#include <iostream>
#include <fstream>
using namespace std;
int main() {
    ifstream check("marker");
    if (check.is_open()) {
        cout << "LEAK\n";
        return 0;
    }
    ofstream create("marker");
    create << "1\n";
    create.close();
    long long a, b;
    if (cin >> a >> b) {
        cout << (a + b) << "\n";
    }
    return 0;
}
"#;
        let sol_marker = aplusb_dir1.join("solutions").join("marker.cpp");
        fs::write(&sol_marker, marker_cpp).unwrap();
        let comp_marker = session.compile(&sol_marker, false).unwrap();

        let p_ctx1 = Arc::new(ProblemContext::prepare(1, &aplusb_dir1, &cfg1, &session).unwrap());
        let p_ctx2 = Arc::new(ProblemContext::prepare(2, &aplusb_dir2, &cfg2, &session).unwrap());

        let tests1 = uj_core::tests::discover_tests(&aplusb_dir1, &cfg1).unwrap();
        let tests2 = uj_core::tests::discover_tests(&aplusb_dir2, &cfg2).unwrap();

        let sub1 = Arc::new(
            SubmissionContext::prepare(1, "sub1", p_ctx1, &comp_marker, tests1, false).unwrap(),
        );

        let sub2 = Arc::new(
            SubmissionContext::prepare(2, "sub2", p_ctx2, &comp_marker, tests2, false).unwrap(),
        );

        // slots: 1, window: 2 to ensure the same worker processes both problems sequentially
        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 1,
            window: 2,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let cancel = CancelToken::new();
        let reports = pool.run(
            vec![
                PoolSubmission {
                    ctx: sub1,
                    events: None,
                    precise: false,
                },
                PoolSubmission {
                    ctx: sub2,
                    events: None,
                    precise: false,
                },
            ],
            &cancel,
        );

        assert_eq!(reports.len(), 2);
        let rep1 = reports[0].as_ref().unwrap();
        let rep2 = reports[1].as_ref().unwrap();
        assert_eq!(rep1.verdict, Verdict::Accepted);
        assert_eq!(rep2.verdict, Verdict::Accepted);

        // Ensure none of the test cases produced LEAK (which would cause WA)
        for t in &rep1.tests {
            assert_eq!(
                t.verdict,
                Verdict::Accepted,
                "Problem 1 test {} leaked: {:?}",
                t.id,
                t.verdict
            );
        }
        for t in &rep2.tests {
            assert_eq!(
                t.verdict,
                Verdict::Accepted,
                "Problem 2 test {} leaked: {:?}",
                t.id,
                t.verdict
            );
        }
    }

    #[test]
    fn test_pool_oversized_task_runs_exclusively() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let p_content = fs::read_to_string(&p_toml).unwrap();
        let cfg = ProblemConfig::from_toml_str(&p_content, Some(&p_toml)).unwrap();

        let session = crate::compiler::CompileSession::new();
        let sol_ac = aplusb_dir.join("solutions").join("ac.cpp");
        let comp_ac = session.compile(&sol_ac, false).unwrap();

        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();

        let sub = Arc::new(
            SubmissionContext::prepare(1, "budget_sub", p_ctx, &comp_ac, tests, false).unwrap(),
        );

        // Test with tight memory budget (128MiB < aplusb's 512MiB memory limit)
        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 4,
            window: 2,
            memory_budget: 128 * 1024 * 1024,
            pool_events: None,
        });

        let cancel = CancelToken::new();
        let reports = pool.run(
            vec![PoolSubmission {
                ctx: sub,
                events: None,
                precise: false,
            }],
            &cancel,
        );

        assert_eq!(reports.len(), 1);
        let rep = reports[0].as_ref().unwrap();
        assert_eq!(rep.verdict, Verdict::Accepted);
    }

    #[test]
    fn test_pool_subtask_dependency_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let prob_dir = tmp.path().join("dep_prob");
        let tests_dir = prob_dir.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();

        let toml_str = r#"
name = "dep_prob"
type = "traditional"
[limits]
time = "1000ms"
memory = "256MB"
[[subtasks]]
id = 1
score = 40.0
tests = ["1_*"]
[[subtasks]]
id = 2
score = 60.0
tests = ["2_*"]
depends = [1]
"#;
        let toml_path = prob_dir.join("problem.toml");
        fs::write(&toml_path, toml_str).unwrap();
        // Subtask 1 input is tiny; Subtask 2 input is larger (would win pure LPT without topo rank)
        fs::write(tests_dir.join("1_1.in"), "1 2\n").unwrap();
        fs::write(tests_dir.join("1_1.ans"), "3\n").unwrap();
        fs::write(tests_dir.join("2_1.in"), "100000 200000\n").unwrap();
        fs::write(tests_dir.join("2_1.ans"), "300000\n").unwrap();

        let sol_src = prob_dir.join("wa_st1.cpp");
        fs::write(
            &sol_src,
            r#"
#include <iostream>
int main() {
    long long a, b;
    if (!(std::cin >> a >> b)) return 0;
    if (a == 1) std::cout << 999 << "\n";
    else std::cout << (a + b) << "\n";
    return 0;
}
"#,
        )
        .unwrap();

        let cfg = ProblemConfig::from_toml_str(toml_str, Some(&toml_path)).unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp = session.compile(&sol_src, false).unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &prob_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&prob_dir, &cfg).unwrap();

        // Notice fail_fast is false: dependency skipping must still skip subtask 2!
        let sub =
            Arc::new(SubmissionContext::prepare(1, "wa_st1", p_ctx, &comp, tests, false).unwrap());

        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 2,
            window: 1,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let cancel = CancelToken::new();
        let reports = pool.run(
            vec![PoolSubmission {
                ctx: sub,
                events: None,
                precise: false,
            }],
            &cancel,
        );
        let rep = reports[0].as_ref().unwrap();
        assert_eq!(rep.score, 0.0);
        let t1 = rep.tests.iter().find(|t| t.id == "1_1").unwrap();
        let t2 = rep.tests.iter().find(|t| t.id == "2_1").unwrap();
        assert_eq!(t1.verdict, Verdict::WrongAnswer);
        assert_eq!(
            t2.verdict,
            Verdict::Skipped,
            "Dependent subtask 2 test should be Skipped when subtask 1 fails"
        );
    }
}

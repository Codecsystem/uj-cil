//! Global judging pool with memory budget and concurrent submission evaluation.
//!
//! Round 14 architecture:
//! - Submissions arrive on a channel ([`JudgePool::run_streaming`]); [`JudgePool::run`] is a
//!   thin wrapper that pre-fills the channel.
//! - A single global ready queue (`BinaryHeap` + `Condvar`) orders tasks by
//!   (admission epoch, input size desc) — global LPT inside FIFO submission admission.
//! - Admission is driven by ready-queue depth (a new submission is admitted whenever fewer
//!   than `worker_count` tasks are ready), not by whole-submission completion.
//! - Dependent subtask tests are only released into the ready queue once all of their
//!   transitive dependencies have completed; if a dependency fails they are emitted as
//!   `Skipped` immediately (VULN-06 cascade). Workers never block waiting on dependencies.
//! - Precise rejudging (Phase C) runs candidates in parallel on a few distinct physical
//!   cores, with one exclusive sandbox per rerun worker and early termination of hopeless
//!   reruns.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Condvar, Mutex};
use uj_core::config::{ProblemType, ScoringType};
use uj_core::run::Tier;
use uj_core::score::score_to_fixed;
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_report::result::{
    JudgeEvent, PoolEvent, SingleTestResult, SubmissionJudgeReport, SubmissionRef,
};
use uj_sandbox::{CancelToken, Sandbox, SlotBoxGuard, create_sandbox};

use crate::budget::{MemoryBudget, task_memory_cost};
use crate::context::{SubmissionContext, run_one_test};
use crate::judge::{aggregate_submission_report, topological_subtask_order};
use crate::scheduler::SchedulerError;
use crate::topology::{Slot, Topology};

#[cfg(test)]
static TEST_PANIC_ON_TEST_NAME: std::sync::LazyLock<parking_lot::Mutex<Option<String>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(None));

const INTERRUPTED_MSG: &str = "Judge execution was interrupted before completion";

/// Number of reruns per precise-rejudge candidate (upper bound; see [`should_stop_reruns`]).
const PRECISE_RERUNS: usize = 2;
/// Maximum number of precise-rejudge candidates per submission.
const PRECISE_MAX_CANDIDATES: usize = 3;
/// A submission with more than this many non-borderline TLEs is treated as genuinely slow.
const GENUINELY_SLOW_TLE_COUNT: usize = 3;

/// Decide whether a precise-rejudge rerun should replace `best_so_far`.
///
/// Rules:
/// 1. Never accept a rerun with `Verdict::JudgeFailed` or `Verdict::Skipped`.
/// 2. Only allow changing the verdict class when `original_verdict == Verdict::TimeLimitExceeded`
///    (to rescue false TLEs caused by parallel contention). Otherwise the rerun must keep the
///    same verdict as `best_so_far`.
/// 3. Accept when the rerun rescues a `TimeLimitExceeded` into a non-TLE verdict, or when it
///    achieves a strictly lower CPU time within the compatible verdict class.
pub(crate) fn should_accept_precise_rerun(
    original_verdict: Verdict,
    best_so_far: &SingleTestResult,
    rerun: &SingleTestResult,
) -> bool {
    if matches!(rerun.verdict, Verdict::JudgeFailed | Verdict::Skipped) {
        return false;
    }
    let verdict_compatible =
        rerun.verdict == best_so_far.verdict || original_verdict == Verdict::TimeLimitExceeded;
    if !verdict_compatible {
        return false;
    }
    if best_so_far.verdict == Verdict::TimeLimitExceeded
        && rerun.verdict != Verdict::TimeLimitExceeded
    {
        return true;
    }
    if best_so_far.verdict != Verdict::TimeLimitExceeded
        && rerun.verdict == Verdict::TimeLimitExceeded
    {
        return false;
    }
    rerun.time_ms < best_so_far.time_ms
}

/// Decide whether the remaining precise reruns of a candidate can be skipped after `rerun`.
///
/// - (a) A clean (non-JudgeFailed) rerun that is still TLE with CPU time ≥ TL cannot be
///   rescued by more reruns on an idle core: stop.
/// - (b) For a genuinely slow submission (many non-borderline TLEs), stop as soon as the
///   verdict is settled to a non-TLE class; the extra rerun would only refine the timing
///   sample, never the score.
pub(crate) fn should_stop_reruns(
    rerun: &SingleTestResult,
    tl_ms: u64,
    genuinely_slow: bool,
) -> bool {
    if matches!(rerun.verdict, Verdict::JudgeFailed | Verdict::Skipped) {
        return false;
    }
    if rerun.verdict == Verdict::TimeLimitExceeded && rerun.time_ms >= tl_ms {
        return true;
    }
    genuinely_slow && rerun.verdict != Verdict::TimeLimitExceeded
}

/// Borderline window used for precise rejudging and early-stop classification.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreciseBounds {
    pub tl_ms: u64,
    pub lower: u64,
    pub upper: u64,
}

impl PreciseBounds {
    pub(crate) fn new(tl_ms: u64) -> Self {
        Self {
            tl_ms,
            lower: (tl_ms as f64 * 0.9) as u64,
            upper: (tl_ms as f64 * 1.5) as u64,
        }
    }

    /// Whether a result would be selected for precise rejudging (before candidate capping).
    pub(crate) fn is_candidate(&self, tr: &SingleTestResult) -> bool {
        if matches!(tr.verdict, Verdict::JudgeFailed | Verdict::Skipped) {
            return false;
        }
        let is_borderline = tr.time_ms >= self.lower && tr.time_ms <= self.upper;
        let is_borderline_tle = tr.verdict == Verdict::TimeLimitExceeded && tr.time_ms < self.upper;
        is_borderline || is_borderline_tle
    }

    /// A failure that zeroes a `min` subtask and that precise rejudging can never change:
    /// zero score and either a non-TLE failure class (reruns never change those verdict
    /// classes) or a TLE outside the rejudge window (or any TLE when precise is off).
    pub(crate) fn is_hard_failure(&self, tr: &SingleTestResult, precise: bool) -> bool {
        if score_to_fixed(tr.score) != 0 {
            return false;
        }
        match tr.verdict {
            Verdict::WrongAnswer
            | Verdict::PresentationError
            | Verdict::RuntimeError
            | Verdict::MemoryLimitExceeded
            | Verdict::OutputLimitExceeded
            | Verdict::RestrictedFunction => true,
            Verdict::TimeLimitExceeded => !precise || !self.is_candidate(tr),
            _ => false,
        }
    }
}

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
    /// Retained for API compatibility. Since Round 14, admission is driven by ready-queue
    /// depth (a submission is admitted whenever fewer than `slots` tasks are ready) instead
    /// of a fixed number of in-flight submissions.
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

// ----------------------------------------------------------------------------
// Per-submission runtime state
// ----------------------------------------------------------------------------

#[derive(Default)]
struct SubSched {
    /// Admission epoch (FIFO order of submissions in the ready queue).
    epoch: u64,
    /// Tests of each subtask that have not completed yet.
    remaining: HashMap<u32, usize>,
    /// Subtasks with at least one non-accepted (incl. skipped) test.
    failed: HashSet<u32>,
    /// `min` subtasks with an early-stop-triggering hard failure.
    hard_failed: HashSet<u32>,
    /// Dependent subtasks whose tests wait for their dependencies (topological order).
    blocked: Vec<(u32, Vec<usize>)>,
}

struct SubRun {
    /// Receipt index inside this pool run.
    idx: usize,
    ctx: Arc<SubmissionContext>,
    event_tx: Option<Sender<JudgeEvent>>,
    precise: bool,
    sub_ref: SubmissionRef,
    bounds: PreciseBounds,
    min_subtasks: HashSet<u32>,
    /// LPT keys (input sizes) per test index, computed off the hot path.
    sizes: Vec<u64>,
    sched: Mutex<SubSched>,
}

impl SubRun {
    fn new(idx: usize, ps: PoolSubmission) -> Self {
        let ctx = ps.ctx;
        let sub_ref = SubmissionRef {
            id: ctx.submission_id,
            problem: ctx.problem.config.name.clone(),
            name: ctx.submission_name.clone(),
        };
        let min_subtasks = ctx
            .problem
            .config
            .subtasks
            .iter()
            .filter(|s| s.scoring == ScoringType::Min)
            .map(|s| s.id)
            .collect();
        let sizes = ctx
            .tests
            .iter()
            .map(TestCase::effective_input_size)
            .collect();
        let mut remaining: HashMap<u32, usize> = ctx
            .problem
            .config
            .subtasks
            .iter()
            .map(|s| (s.id, 0))
            .collect();
        for t in &ctx.tests {
            if let Some(st) = t.subtask_id {
                *remaining.entry(st).or_insert(0) += 1;
            }
        }
        Self {
            idx,
            bounds: PreciseBounds::new(ctx.problem.limits.cpu_time_ms),
            event_tx: ps.events,
            precise: ps.precise,
            sub_ref,
            min_subtasks,
            sizes,
            sched: Mutex::new(SubSched {
                remaining,
                ..Default::default()
            }),
            ctx,
        }
    }

    fn deps_of(&self, subtask: u32) -> &[u32] {
        self.ctx
            .problem
            .subtask_transitive_deps
            .get(&subtask)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Send an event to the per-submission stream and/or the pool stream, cloning only when
    /// both consumers are present.
    fn emit(&self, ev: JudgeEvent, pool_events: Option<&Sender<PoolEvent>>) {
        match (&self.event_tx, pool_events) {
            (Some(tx), Some(ptx)) => {
                let _ = tx.send(ev.clone());
                let _ = ptx.send(PoolEvent {
                    submission: self.sub_ref.clone(),
                    inner: ev,
                });
            }
            (Some(tx), None) => {
                let _ = tx.send(ev);
            }
            (None, Some(ptx)) => {
                let _ = ptx.send(PoolEvent {
                    submission: self.sub_ref.clone(),
                    inner: ev,
                });
            }
            (None, None) => {}
        }
    }

    /// Called at admission: returns test indices that are immediately ready and parks the
    /// tests of dependent subtasks.
    fn admit(&self, epoch: u64) -> Vec<usize> {
        let mut s = self.sched.lock();
        s.epoch = epoch;
        let mut ready = Vec::with_capacity(self.ctx.tests.len());
        let mut blocked: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, t) in self.ctx.tests.iter().enumerate() {
            if self.ctx.problem.has_depends
                && let Some(st) = t.subtask_id
            {
                let deps = self.deps_of(st);
                if !deps.is_empty()
                    && !deps
                        .iter()
                        .all(|d| s.remaining.get(d).copied().unwrap_or(0) == 0)
                {
                    blocked.entry(st).or_default().push(i);
                    continue;
                }
            }
            ready.push(i);
        }
        let ranks = &self.ctx.problem.subtask_topo_rank;
        let mut blocked: Vec<(u32, Vec<usize>)> = blocked.into_iter().collect();
        blocked.sort_by_key(|(st, _)| (ranks.get(st).copied().unwrap_or(usize::MAX), *st));
        s.blocked = blocked;
        ready
    }

    fn has_blocked(&self) -> bool {
        !self.sched.lock().blocked.is_empty()
    }

    /// Reason to skip a ready test right before running it (`--fail-fast` or Min early stop).
    fn pre_run_skip(&self, test: &TestCase) -> Option<&'static str> {
        let st = test.subtask_id?;
        if !self.ctx.fail_fast && !self.ctx.min_early_stop {
            return None;
        }
        let s = self.sched.lock();
        if self.ctx.fail_fast && s.failed.contains(&st) {
            return Some("Skipped due to --fail-fast");
        }
        if self.ctx.min_early_stop && s.hard_failed.contains(&st) {
            return Some(
                "Skipped: an earlier test of this min-scored subtask already failed (early stop)",
            );
        }
        None
    }

    /// Whether `res` should trigger Min-subtask early stop for its subtask.
    fn triggers_early_stop(&self, test: &TestCase, res: &SingleTestResult) -> bool {
        self.ctx.min_early_stop
            && test
                .subtask_id
                .is_some_and(|st| self.min_subtasks.contains(&st))
            && self.bounds.is_hard_failure(res, self.precise)
    }

    /// Record completion of test `idx`. Returns tests released into the ready queue and
    /// synthetic `Skipped` results for tests whose dependencies failed (cascading).
    fn complete(
        &self,
        idx: usize,
        passed: bool,
        hard: bool,
        tier: Option<Tier>,
    ) -> (Vec<usize>, Vec<SingleTestResult>) {
        let Some(st) = self.ctx.tests[idx].subtask_id else {
            return (Vec::new(), Vec::new());
        };
        let mut s = self.sched.lock();
        if let Some(c) = s.remaining.get_mut(&st) {
            *c = c.saturating_sub(1);
        }
        if !passed {
            s.failed.insert(st);
        }
        if hard {
            s.hard_failed.insert(st);
        }
        if s.blocked.is_empty() {
            return (Vec::new(), Vec::new());
        }
        self.resolve_blocked(&mut s, tier)
    }

    fn resolve_blocked(
        &self,
        s: &mut SubSched,
        tier: Option<Tier>,
    ) -> (Vec<usize>, Vec<SingleTestResult>) {
        let mut released = Vec::new();
        let mut skipped = Vec::new();
        loop {
            let mut changed = false;
            let mut i = 0;
            while i < s.blocked.len() {
                let st = s.blocked[i].0;
                let deps = self.deps_of(st);
                if let Some(&failed_dep) = deps.iter().find(|d| s.failed.contains(d)) {
                    let (_, tests) = s.blocked.remove(i);
                    for t_idx in tests {
                        skipped.push(SingleTestResult::synthetic(
                            &self.ctx.tests[t_idx],
                            Verdict::Skipped,
                            Some(format!(
                                "Skipped due to failed dependency subtask {failed_dep}"
                            )),
                            tier,
                        ));
                        if let Some(c) = s.remaining.get_mut(&st) {
                            *c = c.saturating_sub(1);
                        }
                    }
                    s.failed.insert(st);
                    changed = true;
                    continue;
                }
                if deps
                    .iter()
                    .all(|d| s.remaining.get(d).copied().unwrap_or(0) == 0)
                {
                    let (_, tests) = s.blocked.remove(i);
                    released.extend(tests);
                    changed = true;
                    continue;
                }
                i += 1;
            }
            if !changed {
                break;
            }
        }
        (released, skipped)
    }

    /// Deadlock fallback: skip every still-blocked test (should be unreachable because
    /// subtask cycles are rejected by config validation).
    fn force_skip_blocked(&self) -> Vec<SingleTestResult> {
        let mut s = self.sched.lock();
        let blocked = std::mem::take(&mut s.blocked);
        blocked
            .into_iter()
            .flat_map(|(_, tests)| tests)
            .map(|t_idx| {
                SingleTestResult::synthetic(
                    &self.ctx.tests[t_idx],
                    Verdict::Skipped,
                    Some("Skipped due to unsatisfiable subtask dependency".to_string()),
                    None,
                )
            })
            .collect()
    }
}

// ----------------------------------------------------------------------------
// Global ready queue
// ----------------------------------------------------------------------------

struct Queued {
    epoch: u64,
    size: u64,
    seq: u64,
    sub: Arc<SubRun>,
    test_idx: usize,
}

impl Queued {
    fn key(&self) -> (u64, u64, u64) {
        (self.epoch, self.size, self.seq)
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    /// Max-heap order: earliest epoch first, then largest input, then FIFO.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .epoch
            .cmp(&self.epoch)
            .then(self.size.cmp(&other.size))
            .then(other.seq.cmp(&self.seq))
    }
}

#[derive(Default)]
struct QueueState {
    heap: BinaryHeap<Queued>,
    pending: VecDeque<Arc<SubRun>>,
    /// Admitted submissions that had blocked tests at admission (deadlock fallback only).
    with_blocked: Vec<Arc<SubRun>>,
    feeder_done: bool,
    running: usize,
    seq: u64,
    next_epoch: u64,
}

impl QueueState {
    fn push(&mut self, sub: &Arc<SubRun>, test_idx: usize, epoch: u64) {
        self.seq += 1;
        self.heap.push(Queued {
            epoch,
            size: sub.sizes[test_idx],
            seq: self.seq,
            sub: Arc::clone(sub),
            test_idx,
        });
    }

    /// Admit pending submissions while fewer than `worker_count` tasks are ready.
    fn admit(&mut self, worker_count: usize, cancel: &CancelToken) {
        while self.heap.len() < worker_count && !cancel.is_cancelled() {
            let Some(sub) = self.pending.pop_front() else {
                break;
            };
            let epoch = self.next_epoch;
            self.next_epoch += 1;
            let ready = sub.admit(epoch);
            if sub.has_blocked() {
                self.with_blocked.push(Arc::clone(&sub));
            }
            for idx in ready {
                self.push(&sub, idx, epoch);
            }
        }
    }
}

struct Shared {
    state: Mutex<QueueState>,
    cv: Condvar,
    worker_count: usize,
}

enum CollectorMsg {
    Registered(Arc<SubRun>),
    Result {
        sub_idx: usize,
        result: SingleTestResult,
    },
}

struct WorkerEnv {
    shared: Arc<Shared>,
    msg_tx: Sender<CollectorMsg>,
    cancel: CancelToken,
    budget: Arc<MemoryBudget>,
    sandbox_mode: String,
    require_tier: bool,
    pool_events: Option<Sender<PoolEvent>>,
}

impl WorkerEnv {
    fn send_result(&self, sub: &SubRun, result: SingleTestResult) {
        let _ = self.msg_tx.send(CollectorMsg::Result {
            sub_idx: sub.idx,
            result,
        });
    }

    /// Block until a task is ready, or return `None` when the pool is drained / cancelled.
    fn next_task(&self) -> Option<(Arc<SubRun>, usize)> {
        let mut st = self.shared.state.lock();
        loop {
            if self.cancel.is_cancelled() {
                drop(st);
                self.shared.cv.notify_all();
                return None;
            }
            st.admit(self.shared.worker_count, &self.cancel);
            if let Some(q) = st.heap.pop() {
                st.running += 1;
                return Some((q.sub, q.test_idx));
            }
            if st.feeder_done && st.pending.is_empty() && st.running == 0 {
                let stuck = std::mem::take(&mut st.with_blocked);
                drop(st);
                for sub in stuck {
                    for r in sub.force_skip_blocked() {
                        self.send_result(&sub, r);
                    }
                }
                self.shared.cv.notify_all();
                return None;
            }
            // Timed wait only so that cancellation is observed while idle.
            self.shared.cv.wait_for(&mut st, Duration::from_millis(100));
        }
    }

    /// Finish a task: enqueue released dependents, admit more work and wake workers.
    fn finish_task(&self, sub: &Arc<SubRun>, released: Vec<usize>) {
        {
            let mut st = self.shared.state.lock();
            if !released.is_empty() {
                let epoch = sub.sched.lock().epoch;
                for idx in released {
                    st.push(sub, idx, epoch);
                }
            }
            st.running = st.running.saturating_sub(1);
            st.admit(self.shared.worker_count, &self.cancel);
        }
        self.shared.cv.notify_all();
    }

    /// Record a task outcome (dependency bookkeeping, cascaded skips, collector delivery).
    fn record(
        &self,
        sub: &Arc<SubRun>,
        test_idx: usize,
        result: SingleTestResult,
        hard: bool,
        tier: Option<Tier>,
    ) {
        let (released, skipped) = sub.complete(test_idx, result.verdict.is_accepted(), hard, tier);
        for r in skipped {
            self.send_result(sub, r);
        }
        self.send_result(sub, result);
        self.finish_task(sub, released);
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn worker_main(env: WorkerEnv, slot: Slot, initial_sandbox: Option<Box<dyn Sandbox>>) {
    let slot_id = slot.id;
    let cpu_core = slot.cpu_core;

    let sandbox_res = match initial_sandbox {
        Some(sb) => Ok(sb),
        None => create_sandbox(&env.sandbox_mode, env.require_tier),
    };
    let (sandbox, init_error) = match sandbox_res {
        Ok(sb) => (Some(sb), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let tier = sandbox.as_ref().map(|s| s.tier());

    let mut boxes: HashMap<usize, SlotBoxGuard> = HashMap::new();
    // Authorization outcome per prepared executable (success is cached too: the grant lives
    // as long as this worker's sandbox).
    let mut auth_state: HashMap<PathBuf, Result<(), String>> = HashMap::new();

    while let Some((sub, test_idx)) = env.next_task() {
        let test = &sub.ctx.tests[test_idx];

        let Some(sb) = sandbox.as_ref() else {
            let err = init_error.as_deref().unwrap_or("unknown error");
            let failed = SingleTestResult::synthetic(
                test,
                Verdict::JudgeFailed,
                Some(format!("Worker failed to initialize sandbox: {err}")),
                None,
            );
            env.record(&sub, test_idx, failed, false, None);
            continue;
        };

        if let Some(msg) = sub.pre_run_skip(test) {
            let skipped =
                SingleTestResult::synthetic(test, Verdict::Skipped, Some(msg.to_string()), tier);
            env.record(&sub, test_idx, skipped, false, tier);
            continue;
        }

        if let Some(ref p) = sub.ctx.prepared_exe {
            let auth = auth_state
                .entry(p.clone())
                .or_insert_with(|| sb.authorize_executable(p).map_err(|e| e.to_string()));
            if let Err(e) = auth {
                let failed = SingleTestResult::synthetic(
                    test,
                    Verdict::JudgeFailed,
                    Some(format!(
                        "Failed to authorize executable for sandbox slot: {e}"
                    )),
                    tier,
                );
                env.record(&sub, test_idx, failed, false, tier);
                continue;
            }
        }

        let mem_cost = task_memory_cost(&sub.ctx.problem.config, &sub.ctx.problem.limits);
        let Some(lease) = env.budget.acquire(mem_cost, &env.cancel) else {
            // Cancelled while waiting for memory: no result is delivered, so the collector
            // reports the submission as interrupted.
            env.finish_task(&sub, Vec::new());
            break;
        };

        sub.emit(
            JudgeEvent::TestStart {
                id: test.name.clone(),
                subtask: test.subtask_id,
                slot: Some(slot_id),
            },
            env.pool_events.as_ref(),
        );

        // Lazily create one slot box per problem.
        if let std::collections::hash_map::Entry::Vacant(e) =
            boxes.entry(sub.ctx.problem.problem_id)
            && let Ok(box_guard) = uj_sandbox::create_slot_box_dir(&sub.ctx.base_dir_path, slot_id)
        {
            e.insert(box_guard);
        }
        let custom_box_ref = boxes
            .get(&sub.ctx.problem.problem_id)
            .map(|b| b.box_dir.as_path());

        let run_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if let Some(ref target) = *TEST_PANIC_ON_TEST_NAME.lock()
                && &test.name == target
            {
                panic!("Injected worker panic on test {}", test.name);
            }
            run_one_test(
                sb.as_ref(),
                &sub.ctx,
                test,
                cpu_core,
                custom_box_ref,
                &env.cancel,
            )
        }));
        drop(lease);

        let res = match run_outcome {
            Ok(r) => r,
            Err(payload) => SingleTestResult::synthetic(
                test,
                Verdict::JudgeFailed,
                Some(format!(
                    "Worker panic during test execution: {}",
                    panic_message(payload.as_ref())
                )),
                tier,
            ),
        };

        let hard = sub.triggers_early_stop(test, &res);
        sub.emit(JudgeEvent::TestDone(res.clone()), env.pool_events.as_ref());
        env.record(&sub, test_idx, res, hard, tier);
    }
}

// ----------------------------------------------------------------------------
// Pool driver
// ----------------------------------------------------------------------------

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
        let (tx, rx) = unbounded();
        for s in subs {
            let _ = tx.send(s);
        }
        drop(tx);
        self.run_streaming(rx, cancel)
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    /// Streaming variant: submissions are judged as soon as they arrive on `rx` (e.g. right
    /// after their compilation finished). Returns `(submission_id, result)` pairs in the
    /// order the submissions were received. Returns once `rx` is disconnected and all
    /// received submissions are finished.
    pub fn run_streaming(
        &self,
        rx: Receiver<PoolSubmission>,
        cancel: &CancelToken,
    ) -> Vec<(usize, Result<SubmissionJudgeReport, SchedulerError>)> {
        // Pre-validate sandbox initialization and the required tier (handed to worker 0).
        let prechecked_sandbox =
            match create_sandbox(&self.opts.sandbox_mode, self.opts.require_tier) {
                Ok(sb) => sb,
                Err(e) => {
                    let msg = format!("Failed to initialize sandbox: {e}");
                    return rx
                        .iter()
                        .map(|s| {
                            (
                                s.ctx.submission_id,
                                Err(SchedulerError::Sandbox(msg.clone())),
                            )
                        })
                        .collect();
                }
            };

        let topology = Topology::detect(0, Some(self.opts.slots));
        let worker_count = self.opts.slots.max(1);
        let memory_budget = Arc::new(MemoryBudget::new(self.opts.memory_budget));
        let shared = Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            cv: Condvar::new(),
            worker_count,
        });
        let (msg_tx, msg_rx) = unbounded::<CollectorMsg>();

        // Workers
        let mut worker_handles = Vec::with_capacity(worker_count);
        let mut prechecked = Some(prechecked_sandbox);
        for worker_id in 0..worker_count {
            let slot = topology.slots[worker_id % topology.slots.len()].clone();
            let initial_sandbox = if worker_id == 0 {
                prechecked.take()
            } else {
                None
            };
            let env = WorkerEnv {
                shared: Arc::clone(&shared),
                msg_tx: msg_tx.clone(),
                cancel: cancel.clone(),
                budget: Arc::clone(&memory_budget),
                sandbox_mode: self.opts.sandbox_mode.clone(),
                require_tier: self.opts.require_tier,
                pool_events: self.opts.pool_events.clone(),
            };
            worker_handles.push(thread::spawn(move || {
                worker_main(env, slot, initial_sandbox)
            }));
        }

        // Feeder: receives submissions and queues them for admission.
        let feeder_handle = {
            let shared = Arc::clone(&shared);
            let msg_tx = msg_tx.clone();
            let cancel = cancel.clone();
            thread::spawn(move || {
                let mut warned_no_mem_limit = false;
                for (idx, ps) in rx.iter().enumerate() {
                    if ps.ctx.problem.limits.memory_bytes == 0 && !warned_no_mem_limit {
                        warned_no_mem_limit = true;
                        static WARNED_NO_MEM_LIMIT: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !WARNED_NO_MEM_LIMIT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            eprintln!(
                                "Notice: problem has memory limit set to 0 (unlimited memory); memory budget constraint disabled for this problem."
                            );
                        }
                    }
                    let sub = Arc::new(SubRun::new(idx, ps));
                    // Register with the collector before any result of this submission can
                    // exist (crossbeam channels are linearizable FIFO).
                    let _ = msg_tx.send(CollectorMsg::Registered(Arc::clone(&sub)));
                    {
                        let mut st = shared.state.lock();
                        st.pending.push_back(sub);
                        st.admit(shared.worker_count, &cancel);
                    }
                    shared.cv.notify_all();
                }
                shared.state.lock().feeder_done = true;
                shared.cv.notify_all();
            })
        };
        drop(msg_tx);

        // Collector loop (main thread)
        let mut subs: Vec<Arc<SubRun>> = Vec::new();
        let mut results: Vec<Vec<SingleTestResult>> = Vec::new();
        let mut completed: Vec<bool> = Vec::new();
        let mut final_reports: Vec<Option<Result<SubmissionJudgeReport, SchedulerError>>> =
            Vec::new();

        while let Ok(msg) = msg_rx.recv() {
            match msg {
                CollectorMsg::Registered(sub) => {
                    debug_assert_eq!(sub.idx, subs.len());
                    results.push(Vec::with_capacity(sub.ctx.tests.len()));
                    completed.push(sub.ctx.tests.is_empty());
                    final_reports.push(None);
                    subs.push(sub);
                }
                CollectorMsg::Result { sub_idx, result } => {
                    let Some(sub) = subs.get(sub_idx) else {
                        continue;
                    };
                    let list = &mut results[sub_idx];
                    list.push(result);
                    if !completed[sub_idx] && list.len() >= sub.ctx.tests.len() {
                        completed[sub_idx] = true;
                        if !cancel.is_cancelled()
                            && (!sub.precise
                                || sub.ctx.problem.config.problem_type == ProblemType::OutputOnly)
                        {
                            let mut test_results = std::mem::take(list);
                            test_results.sort_by(|a, b| uj_core::tests::natural_cmp(&a.id, &b.id));
                            let report = aggregate_and_dispatch(
                                sub,
                                test_results,
                                self.opts.pool_events.as_ref(),
                            );
                            final_reports[sub_idx] = Some(Ok(report));
                        }
                    }
                }
            }
        }

        let _ = feeder_handle.join();
        for h in worker_handles {
            let _ = h.join();
        }

        // Post-process completed results and categorize precise submissions
        let mut deferred_precise: Vec<(Arc<SubRun>, Vec<SingleTestResult>)> = Vec::new();
        for (sub_idx, sub) in subs.iter().enumerate() {
            if final_reports[sub_idx].is_some() {
                continue;
            }
            let mut test_results = std::mem::take(&mut results[sub_idx]);
            let exp_count = sub.ctx.tests.len();

            if cancel.is_cancelled() || test_results.len() < exp_count {
                final_reports[sub_idx] =
                    Some(Err(SchedulerError::Sandbox(INTERRUPTED_MSG.to_string())));
                continue;
            }

            let all_failed_init = !test_results.is_empty()
                && test_results.iter().all(|r| {
                    r.verdict == Verdict::JudgeFailed
                        && r.message
                            .as_deref()
                            .is_some_and(|m| m.starts_with("Worker failed to initialize sandbox"))
                });

            if all_failed_init {
                if let Some(ref tx) = sub.event_tx {
                    let _ = tx.send(JudgeEvent::Summary {
                        total_time_ms: 0,
                        peak_memory_kb: 0,
                        final_verdict: Verdict::JudgeFailed,
                        final_score: 0.0,
                    });
                }
                final_reports[sub_idx] = Some(Err(SchedulerError::Sandbox(
                    "No test results collected: all sandbox executions failed".to_string(),
                )));
                continue;
            }

            test_results.sort_by(|a, b| uj_core::tests::natural_cmp(&a.id, &b.id));

            if sub.precise && sub.ctx.problem.config.problem_type != ProblemType::OutputOnly {
                deferred_precise.push((Arc::clone(sub), test_results));
            } else {
                let report =
                    aggregate_and_dispatch(sub, test_results, self.opts.pool_events.as_ref());
                final_reports[sub_idx] = Some(Ok(report));
            }
        }

        // Phase C: deferred precise rejudging on idle physical cores
        if !deferred_precise.is_empty() {
            for (sub_idx, res) in self.precise_phase(deferred_precise, &topology, cancel) {
                final_reports[sub_idx] = Some(res);
            }
        }

        subs.iter()
            .zip(final_reports)
            .map(|(sub, rep)| {
                (
                    sub.ctx.submission_id,
                    rep.unwrap_or_else(|| {
                        Err(SchedulerError::Sandbox(INTERRUPTED_MSG.to_string()))
                    }),
                )
            })
            .collect()
    }

    /// Phase C: rerun borderline candidates in exclusive sandboxes on a few distinct physical
    /// cores (`max(1, slots / 4)`), one rerun per core at a time.
    fn precise_phase(
        &self,
        deferred: Vec<(Arc<SubRun>, Vec<SingleTestResult>)>,
        topology: &Topology,
        cancel: &CancelToken,
    ) -> Vec<(usize, Result<SubmissionJudgeReport, SchedulerError>)> {
        let mut jobs: Vec<PreciseJob> = deferred
            .into_iter()
            .map(|(sub, results)| {
                let (candidates, genuinely_slow) = select_precise_candidates(&sub, &results);
                PreciseJob {
                    sub,
                    results,
                    candidates,
                    genuinely_slow,
                    any_tle_flipped_to_ac: false,
                }
            })
            .collect();

        let units: Vec<(usize, usize)> = jobs
            .iter()
            .enumerate()
            .flat_map(|(j, job)| job.candidates.iter().map(move |&idx| (j, idx)))
            .collect();
        let rerun_slots = pick_rerun_slots(topology, units.len());

        if !units.is_empty() && !cancel.is_cancelled() {
            let (unit_tx, unit_rx) = unbounded::<(usize, usize)>();
            for u in &units {
                let _ = unit_tx.send(*u);
            }
            drop(unit_tx);
            let (out_tx, out_rx) = unbounded::<(usize, usize, SingleTestResult, bool)>();
            let jobs_ref = &jobs;
            thread::scope(|scope| {
                for (k, slot) in rerun_slots.iter().enumerate() {
                    let unit_rx = unit_rx.clone();
                    let out_tx = out_tx.clone();
                    let slot = slot.clone();
                    scope.spawn(move || {
                        self.precise_worker(k, &slot, jobs_ref, &unit_rx, &out_tx, cancel);
                    });
                }
            });
            drop(out_tx);
            for (j, idx, best, flipped) in out_rx.iter() {
                jobs[j].results[idx] = best;
                if flipped {
                    jobs[j].any_tle_flipped_to_ac = true;
                }
            }
        }

        // Re-run skipped tests unblocked by a TLE -> AC flip (rare; done serially).
        let mut unblock_env: Option<(Box<dyn Sandbox>, HashMap<PathBuf, SlotBoxGuard>)> = None;
        let unblock_cpu = rerun_slots.first().and_then(|s| s.cpu_core);
        let mut out = Vec::with_capacity(jobs.len());
        for mut job in jobs {
            if cancel.is_cancelled() {
                out.push((
                    job.sub.idx,
                    Err(SchedulerError::Sandbox(INTERRUPTED_MSG.to_string())),
                ));
                continue;
            }
            if job.any_tle_flipped_to_ac
                && job.results.iter().any(|r| r.verdict == Verdict::Skipped)
            {
                if unblock_env.is_none()
                    && let Ok(sb) = create_sandbox(&self.opts.sandbox_mode, self.opts.require_tier)
                {
                    unblock_env = Some((sb, HashMap::new()));
                }
                if let Some((sb, boxes)) = unblock_env.as_mut() {
                    let authorized = match job.sub.ctx.prepared_exe {
                        Some(ref p) => sb.authorize_executable(p).is_ok(),
                        None => true,
                    };
                    if authorized {
                        let base = job.sub.ctx.base_dir_path.clone();
                        if !boxes.contains_key(&base)
                            && let Ok(g) = uj_sandbox::create_slot_box_dir(&base, 9999)
                        {
                            boxes.insert(base.clone(), g);
                        }
                        let rejudge_box = boxes.get(&base).map(|g| g.box_dir.as_path());
                        rerun_unblocked_skipped_tests(
                            &job.sub,
                            &mut job.results,
                            sb.as_ref(),
                            unblock_cpu,
                            rejudge_box,
                            self.opts.pool_events.as_ref(),
                            cancel,
                        );
                    }
                }
            }
            if cancel.is_cancelled() {
                out.push((
                    job.sub.idx,
                    Err(SchedulerError::Sandbox(INTERRUPTED_MSG.to_string())),
                ));
                continue;
            }
            let report =
                aggregate_and_dispatch(&job.sub, job.results, self.opts.pool_events.as_ref());
            out.push((job.sub.idx, Ok(report)));
        }
        out
    }

    fn precise_worker(
        &self,
        k: usize,
        slot: &Slot,
        jobs: &[PreciseJob],
        unit_rx: &Receiver<(usize, usize)>,
        out_tx: &Sender<(usize, usize, SingleTestResult, bool)>,
        cancel: &CancelToken,
    ) {
        // One exclusive sandbox + rejudge box (per problem dir) per rerun worker, created once.
        let sandbox = create_sandbox(&self.opts.sandbox_mode, self.opts.require_tier)
            .map_err(|e| e.to_string());
        let mut boxes: HashMap<PathBuf, Option<SlotBoxGuard>> = HashMap::new();
        let mut auth_state: HashMap<PathBuf, Result<(), String>> = HashMap::new();
        let box_slot_id = 9999 + k;

        while let Ok((j, idx)) = unit_rx.recv() {
            if cancel.is_cancelled() {
                break;
            }
            let job = &jobs[j];
            let sub = &job.sub;
            let orig = &job.results[idx];

            let sb = match sandbox {
                Ok(ref sb) => sb,
                Err(ref e) => {
                    let mut failed = orig.clone();
                    failed.verdict = Verdict::JudgeFailed;
                    failed.message = Some(format!(
                        "Precise rejudge failed: could not create exclusive sandbox: {e}"
                    ));
                    let _ = out_tx.send((j, idx, failed, false));
                    continue;
                }
            };
            if let Some(ref p) = sub.ctx.prepared_exe {
                let auth = auth_state
                    .entry(p.clone())
                    .or_insert_with(|| sb.authorize_executable(p).map_err(|e| e.to_string()));
                if let Err(e) = auth {
                    let mut failed = orig.clone();
                    failed.verdict = Verdict::JudgeFailed;
                    failed.message = Some(format!(
                        "Precise rejudge failed: could not authorize executable: {e}"
                    ));
                    let _ = out_tx.send((j, idx, failed, false));
                    continue;
                }
            }
            let Some(orig_test) = sub.ctx.tests.iter().find(|t| t.name == orig.id) else {
                continue;
            };
            let rejudge_box = boxes
                .entry(sub.ctx.base_dir_path.clone())
                .or_insert_with(|| {
                    uj_sandbox::create_slot_box_dir(&sub.ctx.base_dir_path, box_slot_id).ok()
                })
                .as_ref()
                .map(|g| g.box_dir.as_path());

            let pool_events = self.opts.pool_events.as_ref();
            sub.emit(
                JudgeEvent::TestStart {
                    id: orig.id.clone(),
                    subtask: orig_test.subtask_id,
                    slot: Some(slot.id),
                },
                pool_events,
            );
            sub.emit(
                JudgeEvent::Warning {
                    message: format!("Precise rejudging test {} in exclusive slot...", orig.id),
                    context: Some(orig.id.clone()),
                },
                pool_events,
            );

            let orig_verdict = orig.verdict;
            let mut samples = vec![orig.time_ms];
            let mut best_result = orig.clone();
            for _ in 0..PRECISE_RERUNS {
                if cancel.is_cancelled() {
                    break;
                }
                let rerun = run_one_test(
                    sb.as_ref(),
                    &sub.ctx,
                    orig_test,
                    slot.cpu_core,
                    rejudge_box,
                    cancel,
                );
                if !matches!(rerun.verdict, Verdict::JudgeFailed | Verdict::Skipped) {
                    samples.push(rerun.time_ms);
                }
                let stop = should_stop_reruns(&rerun, sub.bounds.tl_ms, job.genuinely_slow);
                if should_accept_precise_rerun(orig_verdict, &best_result, &rerun) {
                    best_result = rerun;
                }
                if stop {
                    break;
                }
            }

            let min_sample = samples.iter().copied().min().unwrap_or(1).max(1);
            let max_sample = samples.iter().copied().max().unwrap_or(1);
            let variance = (max_sample - min_sample) as f64 / min_sample as f64;
            best_result.timing.rejudged = true;
            best_result.timing.confidence = if variance > 0.10 {
                "low".to_string()
            } else {
                "medium".to_string()
            };
            best_result.timing.samples_ms = samples;
            let flipped = orig_verdict == Verdict::TimeLimitExceeded
                && best_result.verdict == Verdict::Accepted;

            sub.emit(JudgeEvent::TestDone(best_result.clone()), pool_events);
            let _ = out_tx.send((j, idx, best_result, flipped));
        }
    }
}

struct PreciseJob {
    sub: Arc<SubRun>,
    results: Vec<SingleTestResult>,
    candidates: Vec<usize>,
    genuinely_slow: bool,
    any_tle_flipped_to_ac: bool,
}

/// Select precise-rejudge candidates for a submission.
///
/// Returns `(candidate indices, genuinely_slow)`:
/// - Candidates are borderline results (see [`PreciseBounds::is_candidate`]).
/// - (e) Candidates inside a `min` subtask already zeroed by a failure that rejudging can
///   never change are dropped when their rerun cannot change the score or the subtask
///   verdict: non-TLE candidates (reruns never change their verdict class) and TLE
///   candidates when the zeroing verdict has priority ≥ TLE.
/// - At most [`PRECISE_MAX_CANDIDATES`] remain (closest to the TL first).
/// - `genuinely_slow` is set when more than [`GENUINELY_SLOW_TLE_COUNT`] TLEs lie outside
///   the rejudge window (see [`should_stop_reruns`]).
fn select_precise_candidates(sub: &SubRun, results: &[SingleTestResult]) -> (Vec<usize>, bool) {
    let b = &sub.bounds;
    let mut candidates: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| b.is_candidate(r))
        .map(|(i, _)| i)
        .collect();

    // Highest-priority zeroing verdict per min subtask
    let mut zeroed: HashMap<u32, u8> = HashMap::new();
    for r in results {
        let Some(st) = r.subtask else { continue };
        if !sub.min_subtasks.contains(&st) {
            continue;
        }
        let zeroing = score_to_fixed(r.score) == 0
            && match r.verdict {
                Verdict::WrongAnswer
                | Verdict::PresentationError
                | Verdict::RuntimeError
                | Verdict::MemoryLimitExceeded
                | Verdict::OutputLimitExceeded
                | Verdict::RestrictedFunction
                | Verdict::JudgeFailed => true,
                Verdict::TimeLimitExceeded => !b.is_candidate(r),
                _ => false,
            };
        if zeroing {
            let p = zeroed.entry(st).or_insert(0);
            *p = (*p).max(r.verdict.priority());
        }
    }
    if !zeroed.is_empty() {
        candidates.retain(|&i| {
            let r = &results[i];
            let Some(&zero_prio) = r.subtask.and_then(|st| zeroed.get(&st)) else {
                return true;
            };
            r.verdict == Verdict::TimeLimitExceeded
                && zero_prio < Verdict::TimeLimitExceeded.priority()
        });
    }

    if candidates.len() > PRECISE_MAX_CANDIDATES {
        candidates.sort_by_key(|&idx| results[idx].time_ms.abs_diff(b.tl_ms));
        candidates.truncate(PRECISE_MAX_CANDIDATES);
    }

    let hard_tle = results
        .iter()
        .filter(|r| r.verdict == Verdict::TimeLimitExceeded && !b.is_candidate(r))
        .count();
    (candidates, hard_tle > GENUINELY_SLOW_TLE_COUNT)
}

/// Pick up to `max(1, slots / 4)` rerun slots on distinct CPU cores (topology slots map to
/// distinct physical P-cores, so this keeps one rerun per physical core, never SMT siblings).
fn pick_rerun_slots(topology: &Topology, units: usize) -> Vec<Slot> {
    let want = (topology.slots.len() / 4).max(1).min(units.max(1));
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(want);
    for s in &topology.slots {
        if out.len() >= want {
            break;
        }
        if seen.insert(s.cpu_core) {
            out.push(s.clone());
        }
    }
    if out.is_empty() {
        out.push(Slot {
            id: 0,
            cpu_core: None,
        });
    }
    out
}

fn rerun_unblocked_skipped_tests(
    sub: &SubRun,
    test_results: &mut [SingleTestResult],
    exclusive_sandbox: &dyn uj_sandbox::Sandbox,
    exclusive_cpu: Option<usize>,
    rejudge_box: Option<&std::path::Path>,
    pool_events: Option<&Sender<PoolEvent>>,
    cancel: &CancelToken,
) {
    let ctx = &sub.ctx;
    let config = &ctx.problem.config;
    if config.subtasks.is_empty() {
        if !ctx.fail_fast {
            return;
        }
        let mut any_failed = test_results
            .iter()
            .any(|r| r.verdict != Verdict::Skipped && !r.verdict.is_accepted());
        if any_failed {
            return;
        }
        for tr in test_results.iter_mut() {
            if cancel.is_cancelled() || any_failed {
                break;
            }
            if tr.verdict != Verdict::Skipped {
                continue;
            }
            let Some(orig_test) = ctx.tests.iter().find(|t| t.name == tr.id) else {
                continue;
            };
            let fresh = run_one_test(
                exclusive_sandbox,
                ctx,
                orig_test,
                exclusive_cpu,
                rejudge_box,
                cancel,
            );
            if !fresh.verdict.is_accepted() {
                any_failed = true;
            }
            *tr = fresh.clone();
            sub.emit(JudgeEvent::TestDone(fresh), pool_events);
        }
        return;
    }

    let topo = topological_subtask_order(&config.subtasks);
    let mut subtask_passed: HashMap<u32, bool> = HashMap::new();

    for &st_idx in &topo {
        if cancel.is_cancelled() {
            break;
        }
        let st_cfg = &config.subtasks[st_idx];
        let deps_ok = st_cfg
            .depends
            .iter()
            .all(|dep_id| subtask_passed.get(dep_id).copied().unwrap_or(false));
        if !deps_ok {
            subtask_passed.insert(st_cfg.id, false);
            continue;
        }

        let mut st_failed = test_results.iter().any(|r| {
            r.subtask == Some(st_cfg.id)
                && r.verdict != Verdict::Skipped
                && !r.verdict.is_accepted()
        });
        // Min early stop: a hard failure already zeroes this subtask; don't resurrect the
        // tests that were skipped because of it.
        let early_stopped = ctx.min_early_stop
            && st_cfg.scoring == ScoringType::Min
            && test_results.iter().any(|r| {
                r.subtask == Some(st_cfg.id) && sub.bounds.is_hard_failure(r, sub.precise)
            });

        for tr in test_results.iter_mut() {
            if cancel.is_cancelled() {
                break;
            }
            if tr.subtask != Some(st_cfg.id) || tr.verdict != Verdict::Skipped {
                continue;
            }
            if (ctx.fail_fast && st_failed) || early_stopped {
                break;
            }
            let Some(orig_test) = ctx.tests.iter().find(|t| t.name == tr.id) else {
                continue;
            };
            let fresh = run_one_test(
                exclusive_sandbox,
                ctx,
                orig_test,
                exclusive_cpu,
                rejudge_box,
                cancel,
            );
            if !fresh.verdict.is_accepted() {
                st_failed = true;
            }
            *tr = fresh.clone();
            sub.emit(JudgeEvent::TestDone(fresh), pool_events);
        }

        let st_max_fixed = score_to_fixed(st_cfg.score);
        let all_ac = test_results
            .iter()
            .filter(|r| r.subtask == Some(st_cfg.id))
            .all(|r| r.verdict == Verdict::Accepted);
        let earned_fixed: u64 = match st_cfg.scoring {
            ScoringType::Min => test_results
                .iter()
                .filter(|r| r.subtask == Some(st_cfg.id))
                .map(|r| score_to_fixed(r.score).min(st_max_fixed))
                .min()
                .unwrap_or(0),
            ScoringType::Sum => test_results
                .iter()
                .filter(|r| r.subtask == Some(st_cfg.id))
                .map(|r| score_to_fixed(r.score))
                .fold(0u64, u64::saturating_add)
                .min(st_max_fixed),
        };
        subtask_passed.insert(st_cfg.id, all_ac && earned_fixed >= st_max_fixed);
    }
}

fn aggregate_and_dispatch(
    sub: &SubRun,
    test_results: Vec<SingleTestResult>,
    pool_events: Option<&Sender<PoolEvent>>,
) -> SubmissionJudgeReport {
    let ctx = &sub.ctx;
    let report = aggregate_submission_report(
        &ctx.problem.config.name,
        &ctx.submission_name,
        &ctx.problem.config,
        test_results,
    );

    for st in &report.subtasks {
        sub.emit(JudgeEvent::SubtaskDone(st.clone()), pool_events);
    }
    sub.emit(JudgeEvent::SubmissionDone(report.clone()), pool_events);
    sub.emit(
        JudgeEvent::Summary {
            total_time_ms: report.max_time_ms,
            peak_memory_kb: report.max_memory_kb,
            final_verdict: report.verdict,
            final_score: report.score,
        },
        pool_events,
    );

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

    #[test]
    fn test_pool_worker_panic_recovery_no_hang() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let p_content = fs::read_to_string(&p_toml).unwrap();
        let cfg = ProblemConfig::from_toml_str(&p_content, Some(&p_toml)).unwrap();

        let session = crate::compiler::CompileSession::new();
        let sol_ac = aplusb_dir.join("solutions").join("ac.cpp");
        let comp_ac = session.compile(&sol_ac, false).unwrap();

        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();
        let target_test_name = tests[0].name.clone();

        *TEST_PANIC_ON_TEST_NAME.lock() = Some(target_test_name.clone());

        let sub = Arc::new(
            SubmissionContext::prepare(1, "panic_sub", p_ctx, &comp_ac, tests, false).unwrap(),
        );
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

        *TEST_PANIC_ON_TEST_NAME.lock() = None;

        let rep = reports[0].as_ref().unwrap();
        assert_eq!(rep.verdict, Verdict::JudgeFailed);
        let panicked_test = rep
            .tests
            .iter()
            .find(|t| t.id == target_test_name)
            .expect("panicked test should have a synthetic result");
        assert_eq!(panicked_test.verdict, Verdict::JudgeFailed);
        assert!(
            panicked_test
                .message
                .as_deref()
                .is_some_and(|m| m.contains("Worker panic"))
        );
    }

    #[test]
    fn test_precise_rejudge_ignores_judge_failed_and_preserves_verdict_class() {
        let tc = TestCase {
            name: "1".to_string(),
            input_path: PathBuf::from("1.in"),
            answer_path: Some(PathBuf::from("1.ans")),
            subtask_id: None,
            weight: 100_000_000,
            input_size: 0,
        };
        let mut orig_ac = SingleTestResult::synthetic(&tc, Verdict::Accepted, None, None);
        orig_ac.time_ms = 950;
        orig_ac.score = 100.0;

        // A fast JudgeFailed rerun must NEVER overwrite an Accepted result!
        let mut jf_rerun = SingleTestResult::synthetic(
            &tc,
            Verdict::JudgeFailed,
            Some("transient error".to_string()),
            None,
        );
        jf_rerun.time_ms = 1;
        assert!(!should_accept_precise_rerun(
            Verdict::Accepted,
            &orig_ac,
            &jf_rerun
        ));

        // A fast WrongAnswer rerun must not flip an originally Accepted result to WrongAnswer
        let mut wa_rerun = SingleTestResult::synthetic(&tc, Verdict::WrongAnswer, None, None);
        wa_rerun.time_ms = 10;
        assert!(!should_accept_precise_rerun(
            Verdict::Accepted,
            &orig_ac,
            &wa_rerun
        ));

        // A faster Accepted rerun IS accepted when original was Accepted
        let mut faster_ac = orig_ac.clone();
        faster_ac.time_ms = 800;
        assert!(should_accept_precise_rerun(
            Verdict::Accepted,
            &orig_ac,
            &faster_ac
        ));

        // A borderline TLE CAN be rescued by a clean Accepted rerun in the exclusive slot
        let mut orig_tle = SingleTestResult::synthetic(&tc, Verdict::TimeLimitExceeded, None, None);
        orig_tle.time_ms = 1050;
        assert!(should_accept_precise_rerun(
            Verdict::TimeLimitExceeded,
            &orig_tle,
            &faster_ac
        ));
    }

    #[test]
    fn test_pool_cancel_returns_err_not_partial_ok() {
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
        cancel.cancel();

        let sub = Arc::new(
            SubmissionContext::prepare(1, "cancel_sub", p_ctx, &comp_ac, tests, false).unwrap(),
        );
        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 2,
            window: 1,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });

        let reports = pool.run(
            vec![PoolSubmission {
                ctx: sub,
                events: None,
                precise: false,
            }],
            &cancel,
        );
        assert!(
            reports[0].is_err(),
            "Cancelled run must return Err instead of partial Ok report"
        );
    }

    fn synthetic_result(
        name: &str,
        subtask: Option<u32>,
        verdict: Verdict,
        time_ms: u64,
        score: f64,
    ) -> SingleTestResult {
        let tc = TestCase {
            name: name.to_string(),
            input_path: PathBuf::from(format!("{name}.in")),
            answer_path: Some(PathBuf::from(format!("{name}.ans"))),
            subtask_id: subtask,
            weight: 0,
            input_size: 0,
        };
        let mut r = SingleTestResult::synthetic(&tc, verdict, None, None);
        r.time_ms = time_ms;
        r.score = score;
        r
    }

    #[test]
    fn test_queue_order_epoch_then_lpt_then_fifo() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let cfg =
            ProblemConfig::from_toml_str(&fs::read_to_string(&p_toml).unwrap(), Some(&p_toml))
                .unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp = session
            .compile(&aplusb_dir.join("solutions").join("ac.cpp"), false)
            .unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();
        let sub = Arc::new(SubRun::new(
            0,
            PoolSubmission {
                ctx: Arc::new(
                    SubmissionContext::prepare(0, "q", p_ctx, &comp, tests, false).unwrap(),
                ),
                events: None,
                precise: false,
            },
        ));
        let mk = |epoch, size, seq| Queued {
            epoch,
            size,
            seq,
            sub: Arc::clone(&sub),
            test_idx: 0,
        };
        let mut heap = BinaryHeap::new();
        heap.push(mk(1, 1_000, 1));
        heap.push(mk(0, 10, 2));
        heap.push(mk(0, 500, 3));
        heap.push(mk(0, 500, 4));
        heap.push(mk(2, 9_999, 5));
        let order: Vec<(u64, u64, u64)> =
            std::iter::from_fn(|| heap.pop().map(|q| q.key())).collect();
        assert_eq!(
            order,
            vec![
                (0, 500, 3),
                (0, 500, 4),
                (0, 10, 2),
                (1, 1_000, 1),
                (2, 9_999, 5)
            ]
        );
    }

    #[test]
    fn test_should_stop_reruns_early_break() {
        // (a) Clean rerun still TLE with cpu >= TL: stop.
        let tle = synthetic_result("1", None, Verdict::TimeLimitExceeded, 1000, 0.0);
        assert!(should_stop_reruns(&tle, 1000, false));
        // TLE below TL (e.g. wall-clock idleness) may still flip: continue.
        let tle_low = synthetic_result("1", None, Verdict::TimeLimitExceeded, 400, 0.0);
        assert!(!should_stop_reruns(&tle_low, 1000, false));
        // A JudgeFailed rerun never stops the loop.
        let jf = synthetic_result("1", None, Verdict::JudgeFailed, 5000, 0.0);
        assert!(!should_stop_reruns(&jf, 1000, true));
        // AC: keep sampling normally, but stop for genuinely slow submissions (b).
        let ac = synthetic_result("1", None, Verdict::Accepted, 950, 10.0);
        assert!(!should_stop_reruns(&ac, 1000, false));
        assert!(should_stop_reruns(&ac, 1000, true));
    }

    fn min_subtask_problem(tmp: &Path) -> (PathBuf, ProblemConfig) {
        let prob_dir = tmp.join("min_prob");
        let tests_dir = prob_dir.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        let toml_str = r#"
name = "min_prob"
type = "traditional"
[limits]
time = "1000ms"
memory = "256MB"
[[subtasks]]
id = 1
score = 40.0
scoring = "min"
tests = ["1_*"]
[[subtasks]]
id = 2
score = 60.0
scoring = "min"
tests = ["2_*"]
"#;
        let toml_path = prob_dir.join("problem.toml");
        fs::write(&toml_path, toml_str).unwrap();
        // Subtask 1: the failing test (a == 7) has the largest input so it runs first (LPT).
        fs::write(
            tests_dir.join("1_1.in"),
            "7 2                                  \n",
        )
        .unwrap();
        fs::write(tests_dir.join("1_1.ans"), "9\n").unwrap();
        fs::write(tests_dir.join("1_2.in"), "1 2\n").unwrap();
        fs::write(tests_dir.join("1_2.ans"), "3\n").unwrap();
        fs::write(tests_dir.join("1_3.in"), "2 2\n").unwrap();
        fs::write(tests_dir.join("1_3.ans"), "4\n").unwrap();
        fs::write(tests_dir.join("2_1.in"), "5 5\n").unwrap();
        fs::write(tests_dir.join("2_1.ans"), "10\n").unwrap();
        let cfg = ProblemConfig::from_toml_str(toml_str, Some(&toml_path)).unwrap();
        (prob_dir, cfg)
    }

    const WA_ON_SEVEN: &str = r#"
#include <iostream>
int main() {
    long long a, b;
    if (!(std::cin >> a >> b)) return 0;
    if (a == 7) std::cout << 0 << "\n";
    else std::cout << (a + b) << "\n";
    return 0;
}
"#;

    #[test]
    fn test_min_early_stop_preserves_score_and_skips_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (prob_dir, cfg) = min_subtask_problem(tmp.path());
        let sol = prob_dir.join("wa7.cpp");
        fs::write(&sol, WA_ON_SEVEN).unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp = session.compile(&sol, false).unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &prob_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&prob_dir, &cfg).unwrap();

        let run = |early: bool, id: usize| {
            let sub = Arc::new(
                SubmissionContext::prepare(
                    id,
                    "wa7",
                    Arc::clone(&p_ctx),
                    &comp,
                    tests.clone(),
                    false,
                )
                .unwrap()
                .with_min_early_stop(early),
            );
            let pool = JudgePool::new(PoolOptions {
                sandbox_mode: "none".to_string(),
                require_tier: false,
                slots: 1,
                window: 1,
                memory_budget: 1024 * 1024 * 1024,
                pool_events: None,
            });
            pool.run(
                vec![PoolSubmission {
                    ctx: sub,
                    events: None,
                    precise: false,
                }],
                &CancelToken::new(),
            )
            .pop()
            .unwrap()
            .unwrap()
        };

        let full = run(false, 1);
        let early = run(true, 2);
        assert_eq!(score_to_fixed(full.score), score_to_fixed(early.score));
        assert_eq!(score_to_fixed(early.score), score_to_fixed(60.0));
        assert_eq!(full.subtasks[0].score, early.subtasks[0].score);
        assert_eq!(full.subtasks[1].verdict, Verdict::Accepted);
        assert_eq!(early.subtasks[1].verdict, Verdict::Accepted);
        let t11 = early.tests.iter().find(|t| t.id == "1_1").unwrap();
        assert_eq!(t11.verdict, Verdict::WrongAnswer);
        for id in ["1_2", "1_3"] {
            let t = early.tests.iter().find(|t| t.id == id).unwrap();
            assert_eq!(t.verdict, Verdict::Skipped, "{id} should be early-stopped");
            let f = full.tests.iter().find(|t| t.id == id).unwrap();
            assert_eq!(f.verdict, Verdict::Accepted);
        }
        // Non-min-failing subtask is never skipped
        let t21 = early.tests.iter().find(|t| t.id == "2_1").unwrap();
        assert_eq!(t21.verdict, Verdict::Accepted);
        assert_eq!(full.verdict, early.verdict);
    }

    #[test]
    fn test_pool_dependency_chain_released_without_blocking() {
        // A 3-level dependency chain on a single worker: with blocking waits this would
        // rely on topo ordering; with the ready queue dependents are released on completion.
        let tmp = tempfile::tempdir().unwrap();
        let prob_dir = tmp.path().join("chain");
        let tests_dir = prob_dir.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        let toml_str = r#"
name = "chain"
type = "traditional"
[limits]
time = "1000ms"
memory = "256MB"
[[subtasks]]
id = 1
score = 20.0
tests = ["1_*"]
[[subtasks]]
id = 2
score = 30.0
tests = ["2_*"]
depends = [1]
[[subtasks]]
id = 3
score = 50.0
tests = ["3_*"]
depends = [2]
"#;
        let toml_path = prob_dir.join("problem.toml");
        fs::write(&toml_path, toml_str).unwrap();
        for (name, a, b) in [
            ("1_1", 1, 2),
            ("1_2", 3, 4),
            ("2_1", 10, 20),
            ("3_1", 100, 200),
            ("3_2", 5, 6),
        ] {
            // Larger inputs in later subtasks: pure LPT would schedule them first.
            let pad = " ".repeat(if name.starts_with('3') { 64 } else { 0 });
            fs::write(
                tests_dir.join(format!("{name}.in")),
                format!("{a} {b}{pad}\n"),
            )
            .unwrap();
            fs::write(
                tests_dir.join(format!("{name}.ans")),
                format!("{}\n", a + b),
            )
            .unwrap();
        }
        let cfg = ProblemConfig::from_toml_str(toml_str, Some(&toml_path)).unwrap();
        let sol = prob_dir.join("ac.cpp");
        fs::write(
            &sol,
            "#include <iostream>\nint main(){long long a,b; if(std::cin>>a>>b) std::cout<<a+b<<\"\\n\";}\n",
        )
        .unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp = session.compile(&sol, false).unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &prob_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&prob_dir, &cfg).unwrap();
        for slots in [1usize, 3] {
            let sub = Arc::new(
                SubmissionContext::prepare(
                    slots,
                    "ac",
                    Arc::clone(&p_ctx),
                    &comp,
                    tests.clone(),
                    false,
                )
                .unwrap(),
            );
            let pool = JudgePool::new(PoolOptions {
                sandbox_mode: "none".to_string(),
                require_tier: false,
                slots,
                window: 1,
                memory_budget: 1024 * 1024 * 1024,
                pool_events: None,
            });
            let rep = pool
                .run(
                    vec![PoolSubmission {
                        ctx: sub,
                        events: None,
                        precise: false,
                    }],
                    &CancelToken::new(),
                )
                .pop()
                .unwrap()
                .unwrap();
            assert_eq!(rep.verdict, Verdict::Accepted, "slots={slots}");
            assert_eq!(rep.tests.len(), 5);
            assert!(rep.tests.iter().all(|t| t.verdict == Verdict::Accepted));
        }
    }

    #[test]
    fn test_run_streaming_returns_ids_in_receipt_order() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let p_toml = aplusb_dir.join("problem.toml");
        let cfg =
            ProblemConfig::from_toml_str(&fs::read_to_string(&p_toml).unwrap(), Some(&p_toml))
                .unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp_ac = session
            .compile(&aplusb_dir.join("solutions").join("ac.cpp"), false)
            .unwrap();
        let comp_wa = session
            .compile(&aplusb_dir.join("solutions").join("wa.cpp"), false)
            .unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let tests = uj_core::tests::discover_tests(&aplusb_dir, &cfg).unwrap();
        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: "none".to_string(),
            require_tier: false,
            slots: 2,
            window: 1,
            memory_budget: 1024 * 1024 * 1024,
            pool_events: None,
        });
        let (tx, rx) = unbounded();
        let producer = {
            let p_ctx = Arc::clone(&p_ctx);
            let tests = tests.clone();
            std::thread::spawn(move || {
                for (id, comp) in [(42usize, &comp_wa), (7, &comp_ac), (13, &comp_ac)] {
                    let ctx = SubmissionContext::prepare(
                        id,
                        "s",
                        Arc::clone(&p_ctx),
                        comp,
                        tests.clone(),
                        false,
                    )
                    .unwrap();
                    tx.send(PoolSubmission {
                        ctx: Arc::new(ctx),
                        events: None,
                        precise: false,
                    })
                    .unwrap();
                    std::thread::sleep(Duration::from_millis(30));
                }
            })
        };
        let out = pool.run_streaming(rx, &CancelToken::new());
        producer.join().unwrap();
        let ids: Vec<usize> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![42, 7, 13]);
        assert_ne!(out[0].1.as_ref().unwrap().verdict, Verdict::Accepted);
        assert_eq!(out[1].1.as_ref().unwrap().verdict, Verdict::Accepted);
        assert_eq!(out[2].1.as_ref().unwrap().verdict, Verdict::Accepted);
    }

    #[test]
    fn test_select_precise_candidates_skips_zeroed_min_subtasks() {
        let (_tmp, aplusb_dir) = temp_aplusb();
        let toml = r#"
name = "p"
[limits]
time = "1000ms"
memory = "256MB"
[[subtasks]]
id = 1
score = 50.0
scoring = "min"
tests = ["1_*"]
[[subtasks]]
id = 2
score = 50.0
scoring = "sum"
tests = ["2_*"]
"#;
        let cfg = ProblemConfig::from_toml_str(toml, None).unwrap();
        let session = crate::compiler::CompileSession::new();
        let comp = session
            .compile(&aplusb_dir.join("solutions").join("ac.cpp"), false)
            .unwrap();
        let p_ctx = Arc::new(ProblemContext::prepare(0, &aplusb_dir, &cfg, &session).unwrap());
        let sub = SubRun::new(
            0,
            PoolSubmission {
                ctx: Arc::new(
                    SubmissionContext::prepare(0, "s", p_ctx, &comp, Vec::new(), false).unwrap(),
                ),
                events: None,
                precise: true,
            },
        );
        let results = vec![
            // Subtask 1 (min) zeroed by MLE (priority > TLE): borderline TLE and AC are useless.
            synthetic_result("1_1", Some(1), Verdict::MemoryLimitExceeded, 10, 0.0),
            synthetic_result("1_2", Some(1), Verdict::TimeLimitExceeded, 1000, 0.0),
            synthetic_result("1_3", Some(1), Verdict::Accepted, 950, 50.0),
            // Subtask 2 (sum): a TLE flip changes the score, keep it.
            synthetic_result("2_1", Some(2), Verdict::TimeLimitExceeded, 1001, 0.0),
        ];
        let (cands, slow) = select_precise_candidates(&sub, &results);
        assert_eq!(cands, vec![3]);
        assert!(!slow);

        // Zeroed by WA (priority < TLE): a TLE -> AC flip changes the subtask verdict, keep it.
        let results2 = vec![
            synthetic_result("1_1", Some(1), Verdict::WrongAnswer, 10, 0.0),
            synthetic_result("1_2", Some(1), Verdict::TimeLimitExceeded, 1000, 0.0),
            synthetic_result("1_3", Some(1), Verdict::Accepted, 950, 50.0),
        ];
        let (cands2, _) = select_precise_candidates(&sub, &results2);
        assert_eq!(cands2, vec![1]);

        // Genuinely slow: > 3 TLEs far beyond the window.
        let mut results3: Vec<SingleTestResult> = (0..4)
            .map(|i| {
                synthetic_result(
                    &format!("2_{i}"),
                    Some(2),
                    Verdict::TimeLimitExceeded,
                    3000,
                    0.0,
                )
            })
            .collect();
        results3.push(synthetic_result(
            "2_9",
            Some(2),
            Verdict::TimeLimitExceeded,
            1000,
            0.0,
        ));
        let (cands3, slow3) = select_precise_candidates(&sub, &results3);
        assert_eq!(cands3, vec![4]);
        assert!(slow3);
    }
}

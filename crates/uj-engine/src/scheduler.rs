//! Multi-threaded test case scheduler, LPT heuristic queue, and event dispatch.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Sender;

pub use crate::context::{ProblemContext, SubmissionContext, run_one_test};
use uj_core::config::ProblemConfig;
use uj_core::tests::TestCase;
use uj_report::result::{JudgeEvent, SubmissionJudgeReport};
use uj_sandbox::CancelToken;

use crate::compiler::CompileResult;
pub use crate::judge::JudgeOptions;
use crate::pool::{JudgePool, PoolOptions, PoolSubmission};
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
        let limits = problem_config
            .to_limits()
            .map_err(|e| SchedulerError::Config(e.to_string()))?;
        let total_tests = tests.len();
        let topology = Topology::detect(limits.memory_bytes, user_jobs);
        let slots = topology.slots.len().min(total_tests).max(1);

        let problem_ctx = Arc::new(ProblemContext::prepare(
            0,
            problem_dir,
            problem_config,
            &self.session,
        )?);

        let sub_ctx = Arc::new(SubmissionContext::prepare(
            0,
            submission_name,
            problem_ctx,
            compiled,
            tests,
            self.options.fail_fast,
        )?);

        let pool = JudgePool::new(PoolOptions {
            sandbox_mode: self.options.sandbox_mode.clone(),
            require_tier: self.options.require_tier,
            slots,
            window: 1,
            memory_budget: Topology::memory_budget_bytes(),
            pool_events: None,
        });

        let mut reports = pool.run(
            vec![PoolSubmission {
                ctx: sub_ctx,
                events: event_sender,
                precise: self.options.precise,
            }],
            cancel,
        );

        reports.pop().unwrap()
    }
}

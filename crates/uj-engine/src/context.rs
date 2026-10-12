//! Execution contexts and test runner for single test cases.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{CheckerType, ProblemConfig, ProblemType};
use uj_core::run::Limits;
use uj_core::tests::TestCase;
use uj_report::result::SingleTestResult;
use uj_sandbox::{CancelToken, Sandbox};

use crate::compiler::{CompileResult, CompileSession};
use crate::judge::{
    ResolvedChecker, judge_communication_test, judge_interactive_test, judge_output_only_test,
    judge_single_test,
};
use crate::scheduler::SchedulerError;

/// Shared configuration and compiled artifacts for a problem.
pub struct ProblemContext {
    pub problem_id: usize,
    pub problem_dir: PathBuf,
    pub config: ProblemConfig,
    pub limits: Limits,
    pub pe_as_wa: bool,
    pub resolved_checker: ResolvedChecker,
    pub interactor_bin: Option<PathBuf>,
    pub manager_bin: Option<PathBuf>,
}

impl ProblemContext {
    pub fn prepare(
        problem_id: usize,
        problem_dir: &Path,
        problem_config: &ProblemConfig,
        compile_session: &CompileSession,
    ) -> Result<Self, SchedulerError> {
        let limits = problem_config
            .to_limits()
            .map_err(|e| SchedulerError::Config(e.to_string()))?;

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

        Ok(Self {
            problem_id,
            problem_dir: problem_dir.to_path_buf(),
            config: problem_config.clone(),
            limits,
            pe_as_wa,
            resolved_checker,
            interactor_bin,
            manager_bin,
        })
    }
}

/// Evaluation context for a single submission to be judged against tests.
pub struct SubmissionContext {
    pub submission_id: usize,
    pub submission_name: String,
    pub problem: Arc<ProblemContext>,
    pub compiled_binary_path: PathBuf,
    pub interpreter_cmd: Option<Vec<String>>,
    pub prepared_exe: Option<PathBuf>,
    pub base_dir_path: PathBuf,
    pub tests: Vec<TestCase>,
    pub fail_fast: bool,
    pub failed_subtasks: Arc<Mutex<std::collections::HashSet<u32>>>,
}

impl SubmissionContext {
    pub fn prepare(
        id: usize,
        name: &str,
        problem: Arc<ProblemContext>,
        compiled: &CompileResult,
        tests: Vec<TestCase>,
        fail_fast: bool,
    ) -> Result<Self, SchedulerError> {
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
                    .unwrap_or(&problem.problem_dir)
            })
            .unwrap_or(&problem.problem_dir)
            .to_path_buf();

        let prepared_exe = if compiled.interpreter_cmd.is_none()
            && problem.config.problem_type != ProblemType::OutputOnly
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

        Ok(Self {
            submission_id: id,
            submission_name: name.to_string(),
            problem,
            compiled_binary_path: compiled.binary_path.clone(),
            interpreter_cmd: compiled.interpreter_cmd.clone(),
            prepared_exe,
            base_dir_path,
            tests,
            fail_fast,
            failed_subtasks: Arc::new(Mutex::new(std::collections::HashSet::new())),
        })
    }

    pub fn is_subtask_failed(&self, subtask_id: u32) -> bool {
        self.failed_subtasks.lock().contains(&subtask_id)
    }

    pub fn mark_subtask_failed(&self, subtask_id: u32) {
        self.failed_subtasks.lock().insert(subtask_id);
    }
}

/// Run a single test case for a submission against a sandbox.
pub fn run_one_test(
    sandbox: &dyn Sandbox,
    sub: &SubmissionContext,
    test: &TestCase,
    cpu_core: Option<usize>,
    box_dir: Option<&Path>,
    cancel: &CancelToken,
) -> SingleTestResult {
    match sub.problem.config.problem_type {
        ProblemType::Interactive => {
            let int_exe = sub
                .problem
                .interactor_bin
                .as_ref()
                .expect("interactor binary must exist");
            judge_interactive_test(
                sandbox,
                &sub.compiled_binary_path,
                sub.interpreter_cmd.as_deref(),
                sub.prepared_exe.as_deref(),
                int_exe,
                test,
                &sub.problem.limits,
                Some(&sub.problem.resolved_checker),
                sub.problem.pe_as_wa,
                cancel,
                box_dir,
            )
        }
        ProblemType::OutputOnly => judge_output_only_test(
            sandbox,
            &sub.compiled_binary_path,
            test,
            &sub.problem.resolved_checker,
            sub.problem.pe_as_wa,
            cancel,
        ),
        ProblemType::Communication => {
            if let Some(ref mgr_exe) = sub.problem.manager_bin {
                judge_communication_test(
                    sandbox,
                    mgr_exe,
                    &sub.compiled_binary_path,
                    sub.interpreter_cmd.as_deref(),
                    sub.prepared_exe.as_deref(),
                    test,
                    &sub.problem.limits,
                    cancel,
                    box_dir,
                    sub.problem.config.processes.unwrap_or(1),
                    sub.problem.config.communication_topology.as_deref(),
                )
            } else {
                judge_single_test(
                    sandbox,
                    &sub.compiled_binary_path,
                    sub.interpreter_cmd.as_deref(),
                    sub.prepared_exe.as_deref(),
                    test,
                    &sub.problem.limits,
                    &sub.problem.resolved_checker,
                    sub.problem.pe_as_wa,
                    cpu_core,
                    Some(&sub.problem.config.io),
                    cancel,
                    box_dir,
                )
            }
        }
        ProblemType::Traditional | ProblemType::Grader => judge_single_test(
            sandbox,
            &sub.compiled_binary_path,
            sub.interpreter_cmd.as_deref(),
            sub.prepared_exe.as_deref(),
            test,
            &sub.problem.limits,
            &sub.problem.resolved_checker,
            sub.problem.pe_as_wa,
            cpu_core,
            Some(&sub.problem.config.io),
            cancel,
            box_dir,
        ),
    }
}

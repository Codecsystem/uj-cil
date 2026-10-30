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
    pub has_depends: bool,
    pub subtask_topo_rank: std::collections::HashMap<u32, usize>,
    pub subtask_transitive_deps: std::collections::HashMap<u32, Vec<u32>>,
}

impl ProblemContext {
    pub fn prepare(
        problem_id: usize,
        problem_dir: &Path,
        problem_config: &ProblemConfig,
        compile_session: &CompileSession,
    ) -> Result<Self, SchedulerError> {
        problem_config
            .validate(None)
            .map_err(|e| SchedulerError::Config(e.to_string()))?;
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

        let has_depends = problem_config
            .subtasks
            .iter()
            .any(|s| !s.depends.is_empty());
        let topo_order = crate::judge::topological_subtask_order(&problem_config.subtasks);
        let mut subtask_topo_rank = std::collections::HashMap::with_capacity(topo_order.len());
        let mut subtask_transitive_deps =
            std::collections::HashMap::with_capacity(topo_order.len());
        for (rank, &st_idx) in topo_order.iter().enumerate() {
            let st = &problem_config.subtasks[st_idx];
            subtask_topo_rank.insert(st.id, rank);
            let mut deps: Vec<u32> = Vec::new();
            for &dep_id in &st.depends {
                if !deps.contains(&dep_id) {
                    deps.push(dep_id);
                }
                if let Some(trans) = subtask_transitive_deps.get(&dep_id) {
                    for &t_id in trans {
                        if !deps.contains(&t_id) {
                            deps.push(t_id);
                        }
                    }
                }
            }
            subtask_transitive_deps.insert(st.id, deps);
        }

        Ok(Self {
            problem_id,
            problem_dir: problem_dir.to_path_buf(),
            config: problem_config.clone(),
            limits,
            pe_as_wa,
            resolved_checker,
            interactor_bin,
            manager_bin,
            has_depends,
            subtask_topo_rank,
            subtask_transitive_deps,
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
    /// Skip the remaining tests of a `scoring = "min"` subtask once one of its tests has a
    /// non-borderline hard failure (WA/PE/RE/MLE/OLE/RF with zero score, or a TLE that the
    /// precise rejudge could never rescue). The subtask score is 0 either way, so the total
    /// score is unaffected. Off by default; enabled by `uj contest` (opt out with
    /// `--no-early-stop`). See [`SubmissionContext::with_min_early_stop`].
    pub min_early_stop: bool,
    pub failed_subtasks: Arc<Mutex<std::collections::HashSet<u32>>>,
    pub subtask_remaining: Arc<Mutex<std::collections::HashMap<u32, usize>>>,
    pub dep_condvar: Arc<parking_lot::Condvar>,
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

        let mut remaining = std::collections::HashMap::new();
        for st in &problem.config.subtasks {
            remaining.insert(st.id, 0);
        }
        for t in &tests {
            if let Some(st_id) = t.subtask_id {
                *remaining.entry(st_id).or_insert(0) += 1;
            }
        }

        if !problem.config.subtasks.is_empty() && !tests.is_empty() {
            let overlaps = uj_core::tests::detect_subtask_pattern_overlaps(&tests, &problem.config);
            if !overlaps.is_empty() {
                let details: Vec<String> = overlaps
                    .iter()
                    .map(|(t_name, st_ids)| format!("test '{t_name}' matched subtasks {st_ids:?}"))
                    .collect();
                return Err(SchedulerError::Config(format!(
                    "Overlapping subtask test patterns: {} (each test case must belong to a single subtask; use `depends = [...]` to model subtask dependencies)",
                    details.join("; ")
                )));
            }
            for st in &problem.config.subtasks {
                if remaining.get(&st.id).copied().unwrap_or(0) == 0 {
                    return Err(SchedulerError::Config(format!(
                        "Subtask {} matched 0 discovered test cases for patterns {:?}",
                        st.id, st.tests
                    )));
                }
            }
        }

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
            min_early_stop: false,
            failed_subtasks: Arc::new(Mutex::new(std::collections::HashSet::new())),
            subtask_remaining: Arc::new(Mutex::new(remaining)),
            dep_condvar: Arc::new(parking_lot::Condvar::new()),
        })
    }

    /// Builder: enable/disable Min-subtask early stop (see [`SubmissionContext::min_early_stop`]).
    pub fn with_min_early_stop(mut self, enabled: bool) -> Self {
        self.min_early_stop = enabled;
        self
    }

    pub fn is_subtask_failed(&self, subtask_id: u32) -> bool {
        self.failed_subtasks.lock().contains(&subtask_id)
    }

    pub fn mark_subtask_failed(&self, subtask_id: u32) {
        self.failed_subtasks.lock().insert(subtask_id);
        self.dep_condvar.notify_all();
    }

    /// Wait until all transitive dependency subtasks of `subtask_id` have completed, or return
    /// `Some(failed_dep_id)` immediately if any transitive dependency has already failed.
    ///
    /// Note: since Round 14 the [`crate::pool::JudgePool`] no longer calls this (dependent
    /// tests are only released into the ready queue once their dependencies have completed),
    /// so no worker thread ever blocks here. Kept for API compatibility.
    pub fn wait_for_dependencies(&self, subtask_id: u32, cancel: &CancelToken) -> Option<u32> {
        let deps = self.problem.subtask_transitive_deps.get(&subtask_id)?;
        if deps.is_empty() {
            return None;
        }
        let mut rem_guard = self.subtask_remaining.lock();
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            {
                let failed = self.failed_subtasks.lock();
                for &dep_id in deps {
                    if failed.contains(&dep_id) {
                        return Some(dep_id);
                    }
                }
            }
            let all_deps_done = deps
                .iter()
                .all(|dep_id| rem_guard.get(dep_id).copied().unwrap_or(0) == 0);
            if all_deps_done {
                let failed = self.failed_subtasks.lock();
                for &dep_id in deps {
                    if failed.contains(&dep_id) {
                        return Some(dep_id);
                    }
                }
                return None;
            }
            self.dep_condvar
                .wait_for(&mut rem_guard, std::time::Duration::from_millis(20));
        }
    }

    /// Record completion of one test belonging to `subtask_id`, marking the subtask failed if
    /// `!passed` and waking any dependent subtask workers.
    pub fn complete_subtask_test(&self, subtask_id: u32, passed: bool) {
        let mut rem_guard = self.subtask_remaining.lock();
        if !passed {
            self.failed_subtasks.lock().insert(subtask_id);
        }
        if let Some(cnt) = rem_guard.get_mut(&subtask_id) {
            *cnt = cnt.saturating_sub(1);
        }
        self.dep_condvar.notify_all();
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
                cpu_core,
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
                    cpu_core,
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

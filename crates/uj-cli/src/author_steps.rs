//! Single-step execution helpers for authoring workflow (gen, validate, ans)
//! and unified case-granular streaming stages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use uj_core::config::ProblemConfig;
use uj_core::run::{KillReason, Limits, RunSpec, Stdio};
use uj_engine::compiler::CompileSession;
use uj_sandbox::{CancelToken, Sandbox};

use crate::author_cmd::safe_write_file;
use crate::error::EXIT_COMPILE_ERROR;

#[derive(Debug, Clone)]
pub struct GenJob {
    pub idx: u32,
    pub gen_name: String,
    pub gen_args: String,
    pub argv: Vec<std::ffi::OsString>,
    pub target_in: PathBuf,
    pub cache_meta: PathBuf,
    pub hash: String,
}

#[derive(Debug, Clone)]
pub enum StepError {
    Failed(String),
    Runtime(String),
}

#[derive(Debug, Clone)]
pub struct AnsOutcome {
    pub elapsed_ms: u64,
    pub cached: bool,
}

pub fn hash_file_streaming(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn plan_gen_jobs(
    problem_dir: &Path,
    cfg: &ProblemConfig,
    force: bool,
    append: bool,
    session: &CompileSession,
    cancel: &CancelToken,
) -> Result<Vec<GenJob>, ExitCode> {
    if cfg.tests.generators.is_empty() {
        return Ok(Vec::new());
    }

    let tests_dir = problem_dir.join(&cfg.tests.dir);
    let _ = fs::create_dir_all(&tests_dir);

    let mut max_idx = 0u32;
    if append && let Ok(entries) = fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "in")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(num) = stem.parse::<u32>()
            {
                max_idx = max_idx.max(num);
            }
        }
    }

    let mut jobs = Vec::new();
    for gen_spec in &cfg.tests.generators {
        if cancel.is_cancelled() {
            return Err(ExitCode::from(crate::error::EXIT_INTERRUPTED));
        }
        let gen_path = problem_dir.join(&gen_spec.generator);
        let compiled_gen = match session.compile(&gen_path, false) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to compile generator '{}': {e}", gen_path.display());
                return Err(ExitCode::from(EXIT_COMPILE_ERROR));
            }
        };

        let mut base_hasher = blake3::Hasher::new();
        if let Ok(b) = fs::read(&compiled_gen.binary_path) {
            base_hasher.update(&b);
        }
        base_hasher.update(gen_spec.args.as_bytes());

        for i in 0..gen_spec.count {
            if cancel.is_cancelled() {
                return Err(ExitCode::from(crate::error::EXIT_INTERRUPTED));
            }
            max_idx += 1;
            let target_in = tests_dir.join(format!("{max_idx}.in"));

            let mut cache_hasher = base_hasher.clone();
            cache_hasher.update(&i.to_le_bytes());
            let hash = cache_hasher.finalize().to_hex().to_string();

            let cache_meta = problem_dir
                .join(".uj")
                .join("cache")
                .join("gen")
                .join(format!("{hash}.hash"));

            if !force && target_in.exists() && cache_meta.exists() {
                continue;
            }

            let mut argv = vec![compiled_gen.binary_path.as_os_str().to_os_string()];
            for arg in gen_spec.args.split_whitespace() {
                argv.push(std::ffi::OsString::from(arg));
            }
            if gen_spec.count > 1 {
                argv.push(std::ffi::OsString::from(format!("{}", i + 1)));
            }

            jobs.push(GenJob {
                idx: max_idx,
                gen_name: gen_spec.generator.clone(),
                gen_args: gen_spec.args.clone(),
                argv,
                target_in,
                cache_meta,
                hash,
            });
        }
    }

    Ok(jobs)
}

pub fn exec_gen(
    sb: &dyn Sandbox,
    job: &GenJob,
    work_dir: &Path,
    cancel: &CancelToken,
) -> Result<(u64, String), StepError> {
    let start = Instant::now();
    let limits = Limits::new(5000, 512 * 1024 * 1024);
    let spec = RunSpec {
        argv: job.argv.clone(),
        env: vec![],
        work_dir: work_dir.to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let res = match sb.run(&spec, cancel) {
        Ok(r) => r,
        Err(e) => {
            return Err(StepError::Runtime(format!(
                "Generator execution error: {e}"
            )));
        }
    };

    if !res.status.is_success() {
        let err_tail = String::from_utf8_lossy(&res.stderr_tail).to_string();
        return Err(StepError::Failed(err_tail));
    }

    let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
    let in_hash = blake3::hash(&stdout_bytes).to_hex().to_string();
    if let Err(e) = safe_write_file(&job.target_in, &stdout_bytes) {
        return Err(StepError::Runtime(format!(
            "Failed to write {}: {e}",
            job.target_in.display()
        )));
    }
    let _ = safe_write_file(&job.cache_meta, job.hash.as_bytes());
    Ok((start.elapsed().as_millis() as u64, in_hash))
}

pub fn exec_validate(
    sb: &dyn Sandbox,
    val_bin: &Path,
    input: &Path,
    subtask: Option<u32>,
    work_dir: &Path,
    cancel: &CancelToken,
) -> Result<u64, StepError> {
    let start = Instant::now();
    let mut argv = vec![val_bin.as_os_str().to_os_string()];
    if let Some(st_id) = subtask {
        argv.push(std::ffi::OsString::from("--group"));
        argv.push(std::ffi::OsString::from(format!("{st_id}")));
    }

    let limits = Limits::new(5000, 512 * 1024 * 1024);
    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir: work_dir.to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::File(input.to_path_buf()),
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let res = match sb.run(&spec, cancel) {
        Ok(r) => r,
        Err(e) => {
            return Err(StepError::Runtime(format!("Validator runtime error: {e}")));
        }
    };

    if !res.status.is_success() {
        let msg = String::from_utf8_lossy(&res.stderr_tail).trim().to_string();
        return Err(StepError::Failed(msg));
    }

    Ok(start.elapsed().as_millis() as u64)
}

#[allow(clippy::too_many_arguments)]
pub fn exec_ans(
    sb: &dyn Sandbox,
    sol_bin: &Path,
    main_hash: &str,
    input: &Path,
    ans_path: &Path,
    problem_dir: &Path,
    work_dir: &Path,
    precomputed_in_hash: Option<&str>,
    limits: &Limits,
    force: bool,
    cancel: &CancelToken,
) -> Result<AnsOutcome, StepError> {
    let in_hash = match precomputed_in_hash {
        Some(h) => h.to_string(),
        None => match hash_file_streaming(input) {
            Ok(h) => h,
            Err(e) => {
                return Err(StepError::Runtime(format!(
                    "Failed to read input file for hash {}: {e}",
                    input.display()
                )));
            }
        },
    };

    let cache_meta = problem_dir
        .join(".uj")
        .join("cache")
        .join("ans")
        .join(main_hash)
        .join(format!("{in_hash}.ans"));

    if !force && ans_path.exists() && cache_meta.exists() {
        return Ok(AnsOutcome {
            elapsed_ms: 0,
            cached: true,
        });
    }

    let start = Instant::now();
    let spec = RunSpec {
        argv: vec![sol_bin.as_os_str().to_os_string()],
        env: vec![],
        work_dir: work_dir.to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::File(input.to_path_buf()),
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: limits.clone(),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let mut res = match sb.run(&spec, cancel) {
        Ok(r) => r,
        Err(e) => {
            return Err(StepError::Runtime(format!("Execution error: {e}")));
        }
    };

    // Cold-start Defender / scheduling stall retry:
    if res.killed_by == Some(KillReason::WallLimit)
        && !cancel.is_cancelled()
        && (res.cpu_time_us / 1000) < limits.cpu_time_ms
    {
        let mut retry_spec = spec.clone();
        retry_spec.limits.wall_time_ms = retry_spec.limits.wall_time_ms.max(15000);
        if let Ok(retry_res) = sb.run(&retry_spec, cancel) {
            res = retry_res;
        }
    }

    if !res.status.is_success() || res.killed_by.is_some() {
        let reason = res
            .killed_by
            .map(|k| format!("{k:?}"))
            .unwrap_or_else(|| format!("{:?}", res.status));
        return Err(StepError::Failed(reason));
    }

    let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
    if let Err(e) = safe_write_file(ans_path, &stdout_bytes) {
        return Err(StepError::Runtime(format!(
            "Failed to write ans file {}: {e}",
            ans_path.display()
        )));
    }
    let _ = safe_write_file(&cache_meta, &stdout_bytes);
    Ok(AnsOutcome {
        elapsed_ms: start.elapsed().as_millis() as u64,
        cached: false,
    })
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum CaseStagesOutcome {
    Success {
        total_cases: usize,
    },
    Failed {
        stage: uj_tui::pipeline::Stage,
        test_name: String,
        message: String,
        exit_code: ExitCode,
    },
    RuntimeError {
        stage: uj_tui::pipeline::Stage,
        message: String,
    },
    ConfigOrCompileError {
        stage: uj_tui::pipeline::Stage,
        exit_code: ExitCode,
    },
    Interrupted,
}

#[derive(Debug)]
struct CaseJob {
    idx: usize,
    name: String,
    input_path: PathBuf,
    ans_path: PathBuf,
    subtask_id: Option<u32>,
    gen_job: Option<GenJob>,
}

#[derive(Debug)]
enum CaseFailure {
    Gen {
        idx: usize,
        gen_name: String,
        gen_args: String,
        msg: String,
    },
    Validate {
        idx: usize,
        test_name: String,
        msg: String,
    },
    Ans {
        idx: usize,
        test_name: String,
        reason: String,
    },
}

#[derive(Debug)]
#[allow(dead_code)]
enum CaseResult {
    Success {
        idx: usize,
    },
    Failed(CaseFailure),
    RuntimeError {
        stage: uj_tui::pipeline::Stage,
        err: String,
    },
}

pub fn run_case_stages(
    problem_dir: &Path,
    cli: &crate::cli::Cli,
    format: crate::cli::OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> CaseStagesOutcome {
    use crate::cli::OutputFormat;
    use crate::error::{
        EXIT_COMPILE_ERROR, EXIT_JUDGE_FAILED, EXIT_USAGE_OR_CONFIG, handle_config_error,
    };
    use indicatif::{ProgressBar, ProgressStyle};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use uj_core::config::SolutionTag;
    use uj_engine::topology::Topology;
    use uj_sandbox::create_sandbox;
    use uj_tui::pipeline::{CaseStageEvent, Stage, run_pipeline_tui};

    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Gen,
                exit_code: ExitCode::from(EXIT_USAGE_OR_CONFIG),
            };
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            let code = handle_config_error(e, format == OutputFormat::Json);
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Gen,
                exit_code: code,
            };
        }
    };

    if cancel.is_cancelled() {
        return CaseStagesOutcome::Interrupted;
    }

    // 1. Compile generator(s) (inside plan_gen_jobs)
    let gen_jobs = match plan_gen_jobs(problem_dir, &cfg, false, false, session, cancel) {
        Ok(jobs) => jobs,
        Err(code) => {
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Gen,
                exit_code: code,
            };
        }
    };

    // 2. Compile validator (optional)
    let compiled_val = if let Some(val_rel) = &cfg.validator {
        let val_path = problem_dir.join(val_rel);
        match session.compile(&val_path, false) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("Failed to compile validator: {e}");
                return CaseStagesOutcome::ConfigOrCompileError {
                    stage: Stage::Validate,
                    exit_code: ExitCode::from(EXIT_COMPILE_ERROR),
                };
            }
        }
    } else {
        None
    };

    // 3. Compile main solution (required)
    let main_sol = cfg
        .solutions
        .iter()
        .find(|s| s.tag == SolutionTag::Main)
        .or_else(|| {
            cfg.solutions
                .iter()
                .find(|s| s.tag == SolutionTag::Accepted)
        })
        .ok_or_else(|| {
            eprintln!("No main or accepted solution configured to generate answers");
            ExitCode::from(EXIT_USAGE_OR_CONFIG)
        });

    let main_sol = match main_sol {
        Ok(s) => s,
        Err(code) => {
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Ans,
                exit_code: code,
            };
        }
    };

    let sol_path = problem_dir.join(&main_sol.path);
    let compiled_sol = match session.compile_problem_solution(problem_dir, &cfg, &sol_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile main solution: {e}");
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Ans,
                exit_code: ExitCode::from(EXIT_COMPILE_ERROR),
            };
        }
    };

    let main_hash = match hash_file_streaming(&compiled_sol.binary_path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to read binary for hash: {e}");
            return CaseStagesOutcome::ConfigOrCompileError {
                stage: Stage::Ans,
                exit_code: ExitCode::from(EXIT_JUDGE_FAILED),
            };
        }
    };

    // 4. Assemble test cases
    let tests_dir = problem_dir.join(&cfg.tests.dir);
    let _ = fs::create_dir_all(&tests_dir);

    let mut found_inputs: std::collections::BTreeMap<String, PathBuf> =
        std::collections::BTreeMap::new();
    if let Ok(entries) = fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(ext) = path.extension()
                && ext.eq_ignore_ascii_case("in")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                found_inputs.insert(stem.to_string(), path);
            }
        }
    }

    for job in &gen_jobs {
        if let Some(stem) = job.target_in.file_stem().and_then(|s| s.to_str()) {
            found_inputs
                .entry(stem.to_string())
                .or_insert_with(|| job.target_in.clone());
        }
    }

    let mut input_vec: Vec<(String, PathBuf)> = found_inputs.into_iter().collect();
    input_vec.sort_by(|a, b| uj_core::tests::natural_cmp(&a.0, &b.0));

    let mut cases = Vec::with_capacity(input_vec.len());
    for (stem, in_path) in &input_vec {
        let ans_candidate = tests_dir.join(format!("{stem}.ans"));
        let out_candidate = tests_dir.join(format!("{stem}.out"));
        let answer_path = if ans_candidate.is_file() {
            Some(ans_candidate)
        } else if out_candidate.is_file() {
            Some(out_candidate)
        } else {
            None
        };
        cases.push(uj_core::tests::TestCase {
            name: stem.clone(),
            input_path: in_path.clone(),
            answer_path,
            subtask_id: None,
            weight: 0,
        });
    }

    uj_core::tests::assign_subtasks_and_weights(&mut cases, &cfg);

    if cases.is_empty() {
        return CaseStagesOutcome::Success { total_cases: 0 };
    }

    let mut gen_jobs_map: std::collections::HashMap<PathBuf, GenJob> =
        std::collections::HashMap::new();
    for job in gen_jobs {
        gen_jobs_map.insert(job.target_in.clone(), job);
    }

    let mut case_jobs = Vec::with_capacity(cases.len());
    let mut has_gen = Vec::with_capacity(cases.len());
    for (idx, case) in cases.iter().enumerate() {
        let opt_job = gen_jobs_map.remove(&case.input_path);
        has_gen.push(opt_job.is_some());
        case_jobs.push(CaseJob {
            idx,
            name: case.name.clone(),
            input_path: case.input_path.clone(),
            ans_path: case.input_path.with_extension("ans"),
            subtask_id: case.subtask_id,
            gen_job: opt_job,
        });
    }

    let limits = cfg
        .to_limits()
        .unwrap_or_else(|_| Limits::new(1000, 256 * 1024 * 1024));
    let topo = Topology::detect(limits.memory_bytes.max(512 * 1024 * 1024), cli.jobs);
    let num_workers = topo.slots.len().max(1).min(case_jobs.len().max(1));

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<CaseJob>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<CaseResult>();
    let stop_flag = Arc::new(AtomicBool::new(false));

    for job in case_jobs {
        let _ = job_tx.send(job);
    }
    drop(job_tx);

    let (event_tx, tui_handle) = if cli.tui {
        let (etx, erx) = crossbeam_channel::unbounded::<CaseStageEvent>();
        let p_name = cfg.name.clone();
        let c_names: Vec<String> = cases.iter().map(|c| c.name.clone()).collect();
        let h_gen = has_gen.clone();
        let cancel_tui = cancel.clone();
        let handle = std::thread::spawn(move || {
            let _ = run_pipeline_tui(&p_name, &c_names, &h_gen, num_workers, erx, cancel_tui);
        });
        (Some(etx), Some(handle))
    } else {
        (None, None)
    };

    let val_binary = compiled_val.map(|c| c.binary_path);
    let sol_binary =
        uj_sandbox::get_or_prepare_authorized_binary(&compiled_sol.binary_path, problem_dir)
            .unwrap_or_else(|_| compiled_sol.binary_path.clone());
    let prob_dir = problem_dir.to_path_buf();
    let gen_work_dir = tests_dir.clone();
    let run_limits = limits.clone();
    let force = false;

    let mut handles = Vec::new();
    for worker_idx in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let val_bin = val_binary.clone();
        let sol_bin = sol_binary.clone();
        let p_dir = prob_dir.clone();
        let g_dir = gen_work_dir.clone();
        let limits = run_limits.clone();
        let main_h = main_hash.clone();
        let etx = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox("auto", false) {
                Ok(s) => s,
                Err(e) => {
                    let _ = res_tx.send(CaseResult::RuntimeError {
                        stage: Stage::Ans,
                        err: format!("Sandbox error: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };

            let slot_box = match uj_sandbox::create_slot_box_dir(&p_dir, worker_idx) {
                Ok(b) => b,
                Err(e) => {
                    let _ = res_tx.send(CaseResult::RuntimeError {
                        stage: Stage::Ans,
                        err: format!("Failed to create sandbox box dir: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };

            if let Some(ref vb) = val_bin {
                let _ = sandbox.authorize_executable(vb);
            }
            let _ = sandbox.authorize_executable(&sol_bin);

            while let Ok(job) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                let mut gen_in_hash: Option<String> = None;

                // 1. Stage: Gen
                if let Some(ref g_job) = job.gen_job {
                    if let Some(ref tx) = etx {
                        let _ = tx.send(CaseStageEvent::Started {
                            idx: job.idx,
                            stage: Stage::Gen,
                        });
                    }

                    match exec_gen(sandbox.as_ref(), g_job, &g_dir, &cancel) {
                        Ok((gen_ms, in_hash)) => {
                            gen_in_hash = Some(in_hash);
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::StageDone {
                                    idx: job.idx,
                                    stage: Stage::Gen,
                                    ms: gen_ms,
                                    cached: false,
                                });
                            }
                        }
                        Err(StepError::Failed(msg)) => {
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::Failed {
                                    idx: job.idx,
                                    stage: Stage::Gen,
                                    message: msg.clone(),
                                });
                            }
                            let _ = res_tx.send(CaseResult::Failed(CaseFailure::Gen {
                                idx: job.idx,
                                gen_name: g_job.gen_name.clone(),
                                gen_args: g_job.gen_args.clone(),
                                msg,
                            }));
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(StepError::Runtime(err)) => {
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::Failed {
                                    idx: job.idx,
                                    stage: Stage::Gen,
                                    message: err.clone(),
                                });
                            }
                            let _ = res_tx.send(CaseResult::RuntimeError {
                                stage: Stage::Gen,
                                err,
                            });
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }

                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                // 2. Stage: Validate
                if let Some(ref vb) = val_bin {
                    if let Some(ref tx) = etx {
                        let _ = tx.send(CaseStageEvent::Started {
                            idx: job.idx,
                            stage: Stage::Validate,
                        });
                    }

                    match exec_validate(
                        sandbox.as_ref(),
                        vb,
                        &job.input_path,
                        job.subtask_id,
                        &p_dir,
                        &cancel,
                    ) {
                        Ok(val_ms) => {
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::StageDone {
                                    idx: job.idx,
                                    stage: Stage::Validate,
                                    ms: val_ms,
                                    cached: false,
                                });
                            }
                        }
                        Err(StepError::Failed(msg)) => {
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::Failed {
                                    idx: job.idx,
                                    stage: Stage::Validate,
                                    message: msg.clone(),
                                });
                            }
                            let _ = res_tx.send(CaseResult::Failed(CaseFailure::Validate {
                                idx: job.idx,
                                test_name: job.name.clone(),
                                msg,
                            }));
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(StepError::Runtime(err)) => {
                            if let Some(ref tx) = etx {
                                let _ = tx.send(CaseStageEvent::Failed {
                                    idx: job.idx,
                                    stage: Stage::Validate,
                                    message: err.clone(),
                                });
                            }
                            let _ = res_tx.send(CaseResult::RuntimeError {
                                stage: Stage::Validate,
                                err,
                            });
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                } else if let Some(ref tx) = etx {
                    let _ = tx.send(CaseStageEvent::StageDone {
                        idx: job.idx,
                        stage: Stage::Validate,
                        ms: 0,
                        cached: false,
                    });
                }

                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                // 3. Stage: Ans
                if let Some(ref tx) = etx {
                    let _ = tx.send(CaseStageEvent::Started {
                        idx: job.idx,
                        stage: Stage::Ans,
                    });
                }

                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                match exec_ans(
                    sandbox.as_ref(),
                    &sol_bin,
                    &main_h,
                    &job.input_path,
                    &job.ans_path,
                    &p_dir,
                    &slot_box.box_dir,
                    gen_in_hash.as_deref(),
                    &limits,
                    force,
                    &cancel,
                ) {
                    Ok(outcome) => {
                        if let Some(ref tx) = etx {
                            let _ = tx.send(CaseStageEvent::StageDone {
                                idx: job.idx,
                                stage: Stage::Ans,
                                ms: outcome.elapsed_ms,
                                cached: outcome.cached,
                            });
                        }
                        let _ = res_tx.send(CaseResult::Success { idx: job.idx });
                    }
                    Err(StepError::Failed(reason)) => {
                        if let Some(ref tx) = etx {
                            let _ = tx.send(CaseStageEvent::Failed {
                                idx: job.idx,
                                stage: Stage::Ans,
                                message: reason.clone(),
                            });
                        }
                        let _ = res_tx.send(CaseResult::Failed(CaseFailure::Ans {
                            idx: job.idx,
                            test_name: job.name.clone(),
                            reason,
                        }));
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(StepError::Runtime(err)) => {
                        if let Some(ref tx) = etx {
                            let _ = tx.send(CaseStageEvent::Failed {
                                idx: job.idx,
                                stage: Stage::Ans,
                                message: err.clone(),
                            });
                        }
                        let _ = res_tx.send(CaseResult::RuntimeError {
                            stage: Stage::Ans,
                            err,
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        handles.push(handle);
    }
    drop(res_tx);
    drop(event_tx);

    let mut success_count = 0;
    let mut first_failure: Option<CaseFailure> = None;
    let mut runtime_error: Option<(Stage, String)> = None;

    let pb = if format == OutputFormat::Human && !cli.quiet && !cli.tui && !cases.is_empty() {
        let p = ProgressBar::new(cases.len() as u64);
        p.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} Running pipeline [{bar:25.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    while let Ok(res) = res_rx.recv() {
        match res {
            CaseResult::Success { .. } => {
                success_count += 1;
                if let Some(ref p) = pb {
                    p.inc(1);
                }
            }
            CaseResult::Failed(fail) => {
                let f_idx = match &fail {
                    CaseFailure::Gen { idx, .. }
                    | CaseFailure::Validate { idx, .. }
                    | CaseFailure::Ans { idx, .. } => *idx,
                };
                if let Some(ref cur) = first_failure {
                    let cur_idx = match cur {
                        CaseFailure::Gen { idx, .. }
                        | CaseFailure::Validate { idx, .. }
                        | CaseFailure::Ans { idx, .. } => *idx,
                    };
                    if f_idx < cur_idx {
                        first_failure = Some(fail);
                    }
                } else {
                    first_failure = Some(fail);
                }
            }
            CaseResult::RuntimeError { stage, err } => {
                if runtime_error.is_none() {
                    runtime_error = Some((stage, err));
                }
            }
        }
    }

    for h in handles {
        let _ = h.join();
    }

    if let Some(p) = pb {
        p.finish_and_clear();
    }

    if let Some(h) = tui_handle {
        let _ = h.join();
    }

    if cancel.is_cancelled() {
        return CaseStagesOutcome::Interrupted;
    }

    if let Some((stage, err)) = runtime_error {
        return CaseStagesOutcome::RuntimeError {
            stage,
            message: err,
        };
    }

    if let Some(fail) = first_failure {
        match fail {
            CaseFailure::Gen {
                gen_name,
                gen_args,
                msg,
                ..
            } => {
                return CaseStagesOutcome::Failed {
                    stage: Stage::Gen,
                    test_name: gen_name.clone(),
                    message: format!("Generator '{gen_name}' failed with args '{gen_args}': {msg}"),
                    exit_code: ExitCode::from(EXIT_JUDGE_FAILED),
                };
            }
            CaseFailure::Validate { test_name, msg, .. } => {
                return CaseStagesOutcome::Failed {
                    stage: Stage::Validate,
                    test_name: test_name.clone(),
                    message: format!("Validator failed on test '{test_name}': {msg}"),
                    exit_code: ExitCode::from(EXIT_JUDGE_FAILED),
                };
            }
            CaseFailure::Ans {
                test_name, reason, ..
            } => {
                return CaseStagesOutcome::Failed {
                    stage: Stage::Ans,
                    test_name: test_name.clone(),
                    message: format!("Main solution failed on test '{test_name}': {reason}"),
                    exit_code: ExitCode::from(EXIT_JUDGE_FAILED),
                };
            }
        }
    }

    if format == OutputFormat::Human && !cli.quiet && !cli.tui {
        println!(
            "All {success_count} test case(s) successfully generated, validated, and answered"
        );
    }

    CaseStagesOutcome::Success {
        total_cases: cases.len(),
    }
}

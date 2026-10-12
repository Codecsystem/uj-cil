//! Authoring workflow commands: init, build, gen, validate, ans, verify, tl, stress, pipeline.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{ProblemConfig, SolutionConfig, SolutionTag};
use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_engine::compiler::CompileSession;
use uj_engine::context::{ProblemContext, SubmissionContext};
use uj_engine::judge::JudgeOptions;
use uj_engine::pool::{JudgePool, PoolOptions, PoolSubmission};
use uj_engine::scheduler::Scheduler;
use uj_report::result::{JudgeEvent, PoolEvent, SubmissionJudgeReport, SubmissionRef};
use uj_sandbox::{CancelToken, create_sandbox};
use uj_tui::multi::{SubmissionMeta, run_multi_tui};

use crate::cli::{Cli, OutputFormat, SandboxMode};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS,
    EXIT_USAGE_OR_CONFIG, EXIT_VERIFY_MISMATCH, handle_config_error,
};

const VENDORED_TESTLIB_H: &str = include_str!("../../../assets/testlib.h");

pub(crate) fn safe_write_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, content) {
        Ok(()) => Ok(()),
        Err(e) => {
            let mut last_err = e;
            for attempt in 0..5 {
                std::thread::sleep(Duration::from_millis(25 * (attempt + 1)));
                match fs::write(path, content) {
                    Ok(()) => return Ok(()),
                    Err(retry_err) => {
                        last_err = retry_err;
                    }
                }
            }
            let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
            let tmp_path = path.with_file_name(format!(".{filename}.{}.tmp", std::process::id()));
            if let Ok(()) = fs::write(&tmp_path, content) {
                for attempt in 0..5 {
                    if fs::rename(&tmp_path, path).is_ok() {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(25 * (attempt + 1)));
                }
                let _ = fs::remove_file(&tmp_path);
            }
            Err(last_err)
        }
    }
}

// ============================================================================
// 1. uj init
// ============================================================================

pub fn run_init(dir: &Path, problem_type: &str, format: OutputFormat) -> ExitCode {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "sample_problem".to_string());

    if let Err(e) = fs::create_dir_all(dir.join("solutions")) {
        eprintln!("Failed to create solutions dir: {e}");
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }
    if let Err(e) = fs::create_dir_all(dir.join("tests")) {
        eprintln!("Failed to create tests dir: {e}");
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    // Write problem.toml
    let problem_toml = format!(
        r#"schema = 1
name = "{name}"
type = "{problem_type}"
validator = "validator.cpp"

[limits]
time = "1.0s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[tests]
dir = "tests"
pattern = "*.in"

[[tests.gen]]
generator = "gen.cpp"
args = "100"
count = 3
subtask = 1

[[subtasks]]
id = 1
score = 100.0
scoring = "min"
tests = ["*"]

[[solutions]]
path = "solutions/std.cpp"
tag = "main"

[[solutions]]
path = "solutions/brute.cpp"
tag = "accepted"

[[solutions]]
path = "solutions/wrong.cpp"
tag = "wrong_answer"
"#
    );
    let _ = fs::write(dir.join("problem.toml"), problem_toml);

    // Write statement.md
    let statement = format!(
        r#"# {name}

## Description
Given two integers $A$ and $B$, compute their sum $A + B$.

## Input Format
The first line contains two integers $A$ and $B$.

## Output Format
Print a single integer representing $A + B$.

## Constraints
- $0 \le A, B \le 10^9$

## Example
```
1 2
```
Output:
```
3
```
"#
    );
    let _ = fs::write(dir.join("statement.md"), statement);

    // Vendored testlib.h
    let _ = fs::write(dir.join("testlib.h"), VENDORED_TESTLIB_H);

    // Generator
    let gen_cpp = r#"#include "testlib.h"
#include <iostream>

using namespace std;

int main(int argc, char* argv[]) {
    registerGen(argc, argv, 1);
    int n = opt<int>(1, 100);
    int a = rnd.next(0, n);
    int b = rnd.next(0, n);
    cout << a << " " << b << endl;
    return 0;
}
"#;
    let _ = fs::write(dir.join("gen.cpp"), gen_cpp);

    // Validator
    let val_cpp = r#"#include "testlib.h"
#include <iostream>

using namespace std;

int main(int argc, char* argv[]) {
    registerValidation(argc, argv);
    inf.readInt(0, 1000000000, "a");
    inf.readSpace();
    inf.readInt(0, 1000000000, "b");
    inf.readEoln();
    inf.readEof();
    return 0;
}
"#;
    let _ = fs::write(dir.join("validator.cpp"), val_cpp);

    // Solutions
    let std_cpp = r#"#include <iostream>
using namespace std;

int main() {
    long long a, b;
    if (cin >> a >> b) {
        cout << a + b << endl;
    }
    return 0;
}
"#;
    let _ = fs::write(dir.join("solutions").join("std.cpp"), std_cpp);

    let brute_cpp = r#"#include <iostream>
using namespace std;

int main() {
    long long a, b;
    if (cin >> a >> b) {
        long long ans = 0;
        for (long long i = 0; i < a; ++i) ans++;
        for (long long i = 0; i < b; ++i) ans++;
        cout << ans << endl;
    }
    return 0;
}
"#;
    let _ = fs::write(dir.join("solutions").join("brute.cpp"), brute_cpp);

    let wrong_cpp = r#"#include <iostream>
using namespace std;

int main() {
    long long a, b;
    if (cin >> a >> b) {
        cout << a + b + 1 << endl;
    }
    return 0;
}
"#;
    let _ = fs::write(dir.join("solutions").join("wrong.cpp"), wrong_cpp);

    // Sample tests
    let _ = fs::write(dir.join("tests").join("1.in"), "1 2\n");
    let _ = fs::write(dir.join("tests").join("1.ans"), "3\n");

    if format == OutputFormat::Json {
        let res = serde_json::json!({
            "ok": true,
            "problem": name,
            "dir": dir.to_string_lossy()
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        println!("Initialized problem '{}' in '{}'", name, dir.display());
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 2. uj build
// ============================================================================

pub fn run_build(
    problem_dir: &Path,
    targets: &[String],
    no_cache: bool,
    format: OutputFormat,
) -> ExitCode {
    let session = CompileSession::new();
    run_build_with_session(problem_dir, targets, no_cache, format, &session)
}

pub fn run_build_with_session(
    problem_dir: &Path,
    targets: &[String],
    no_cache: bool,
    format: OutputFormat,
    compile_session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    let mut to_compile: Vec<PathBuf> = Vec::new();

    if targets.is_empty() {
        if let Some(ref v) = cfg.validator {
            to_compile.push(if v.is_absolute() {
                v.clone()
            } else {
                problem_dir.join(v)
            });
        }
        if let Some(ref chk) = cfg.checker.path {
            to_compile.push(if chk.is_absolute() {
                chk.clone()
            } else {
                problem_dir.join(chk)
            });
        }
        if let Some(ref int) = cfg.interactor {
            to_compile.push(if int.is_absolute() {
                int.clone()
            } else {
                problem_dir.join(int)
            });
        }
        if let Some(ref mgr) = cfg.manager {
            to_compile.push(if mgr.is_absolute() {
                mgr.clone()
            } else {
                problem_dir.join(mgr)
            });
        }
        for g in &cfg.tests.generators {
            let p = problem_dir.join(&g.generator);
            if p.exists() && !to_compile.contains(&p) {
                to_compile.push(p);
            }
        }
        for sol in &cfg.solutions {
            let p = if sol.path.is_absolute() {
                sol.path.clone()
            } else {
                problem_dir.join(&sol.path)
            };
            if p.exists() && !to_compile.contains(&p) {
                to_compile.push(p);
            }
        }
    } else {
        for t in targets {
            to_compile.push(problem_dir.join(t));
        }
    }

    to_compile.retain(|target| {
        !target
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    });

    if to_compile.is_empty() {
        if format == OutputFormat::Json {
            let res = serde_json::json!({ "ok": true, "status": "Build succeeded" });
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
        .min(to_compile.len());

    enum BuildMsg {
        Success { target: PathBuf, duration_ms: u64 },
        Failed { target: PathBuf, error: String },
    }

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<PathBuf>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<BuildMsg>();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    for target in to_compile {
        let _ = job_tx.send(target);
    }
    drop(job_tx);

    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let session = compile_session.clone();

        let handle = std::thread::spawn(move || {
            while let Ok(target) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match session.compile(&target, no_cache) {
                    Ok(c) => {
                        let _ = res_tx.send(BuildMsg::Success {
                            target,
                            duration_ms: c.duration_ms,
                        });
                    }
                    Err(e) => {
                        let _ = res_tx.send(BuildMsg::Failed {
                            target,
                            error: e.to_string(),
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

    let mut first_error: Option<(PathBuf, String)> = None;
    while let Ok(res) = res_rx.recv() {
        match res {
            BuildMsg::Success {
                target,
                duration_ms,
            } => {
                if format == OutputFormat::Human {
                    println!("Compiled {} ({} ms)", target.display(), duration_ms);
                }
            }
            BuildMsg::Failed { target, error } => {
                if first_error.is_none() {
                    first_error = Some((target, error));
                }
            }
        }
    }

    for h in handles {
        let _ = h.join();
    }

    if let Some((target, e)) = first_error {
        if format == OutputFormat::Json {
            let err = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "CompileError",
                    "target": target.to_string_lossy(),
                    "message": e
                }
            });
            println!("{}", serde_json::to_string_pretty(&err).unwrap());
        } else {
            eprintln!("Failed to compile {}: {e}", target.display());
        }
        return ExitCode::from(EXIT_COMPILE_ERROR);
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "status": "Build succeeded" });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 3. uj gen
// ============================================================================

pub fn run_gen(
    problem_dir: &Path,
    force: bool,
    append: bool,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_gen_with_session(problem_dir, force, append, format, cancel, &session)
}

pub fn run_gen_with_session(
    problem_dir: &Path,
    force: bool,
    append: bool,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    if cfg.tests.generators.is_empty() {
        if format == OutputFormat::Human {
            println!("No generator configured in problem.toml");
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    let tests_dir = problem_dir.join(&cfg.tests.dir);
    let jobs =
        match crate::author_steps::plan_gen_jobs(problem_dir, &cfg, force, append, session, cancel)
        {
            Ok(j) => j,
            Err(code) => return code,
        };

    if jobs.is_empty() {
        if format == OutputFormat::Json {
            let res = serde_json::json!({ "ok": true, "generated": 0 });
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        } else if format == OutputFormat::Human {
            println!("All tests already generated or cached");
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    use crate::author_steps::{GenJob, StepError, exec_gen};

    enum GenResult {
        Generated,
        Failed {
            idx: u32,
            gen_name: String,
            gen_args: String,
            msg: String,
        },
        Error {
            err: String,
        },
    }

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
        .min(jobs.len().max(1));

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<GenJob>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<GenResult>();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let total_jobs = jobs.len();
    for job in jobs {
        let _ = job_tx.send(job);
    }
    drop(job_tx);

    let t_dir = tests_dir.clone();
    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let work_dir = t_dir.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox("auto", false) {
                Ok(s) => s,
                Err(e) => {
                    let _ = res_tx.send(GenResult::Error {
                        err: format!("Sandbox error: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };

            while let Ok(job) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                match exec_gen(sandbox.as_ref(), &job, &work_dir, &cancel) {
                    Ok(_) => {
                        let _ = res_tx.send(GenResult::Generated);
                    }
                    Err(StepError::Failed(msg)) => {
                        let _ = res_tx.send(GenResult::Failed {
                            idx: job.idx,
                            gen_name: job.gen_name,
                            gen_args: job.gen_args,
                            msg,
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(StepError::Runtime(err)) => {
                        let _ = res_tx.send(GenResult::Error { err });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        handles.push(handle);
    }
    drop(res_tx);

    let pb = if format == OutputFormat::Human && total_jobs > 0 {
        let p = ProgressBar::new(total_jobs as u64);
        p.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} Generating tests [{bar:25.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    let mut generated_count = 0;
    let mut first_failure: Option<(u32, String, String, String)> = None;
    let mut runtime_error: Option<String> = None;

    while let Ok(res) = res_rx.recv() {
        match res {
            GenResult::Generated => {
                generated_count += 1;
                if let Some(ref p) = pb {
                    p.inc(1);
                }
            }
            GenResult::Failed {
                idx,
                gen_name,
                gen_args,
                msg,
            } => {
                if let Some((f_idx, _, _, _)) = first_failure {
                    if idx < f_idx {
                        first_failure = Some((idx, gen_name, gen_args, msg));
                    }
                } else {
                    first_failure = Some((idx, gen_name, gen_args, msg));
                }
            }
            GenResult::Error { err } => {
                if runtime_error.is_none() {
                    runtime_error = Some(err);
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

    if cancel.is_cancelled() {
        return ExitCode::from(EXIT_INTERRUPTED);
    }

    if let Some(err) = runtime_error {
        eprintln!("{err}");
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, gen_name, gen_args, msg)) = first_failure {
        eprintln!("Generator '{gen_name}' failed with args '{gen_args}': {msg}");
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "generated": generated_count });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        println!(
            "Generated {generated_count} test case(s) in '{}'",
            tests_dir.display()
        );
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 4. uj validate
// ============================================================================

pub fn run_validate(problem_dir: &Path, format: OutputFormat, cancel: &CancelToken) -> ExitCode {
    let session = CompileSession::new();
    run_validate_with_session(problem_dir, format, cancel, &session)
}

pub fn run_validate_with_session(
    problem_dir: &Path,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    let Some(ref val_path_rel) = cfg.validator else {
        if format == OutputFormat::Human {
            println!("No validator configured in problem.toml");
        }
        return ExitCode::from(EXIT_SUCCESS);
    };

    let val_path = problem_dir.join(val_path_rel);
    let compiled_val = match session.compile(&val_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile validator: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to discover tests: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
        .min(tests.len().max(1));

    if tests.is_empty() {
        if format == OutputFormat::Json {
            let res = serde_json::json!({ "ok": true, "validated": 0 });
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        } else if format == OutputFormat::Human {
            println!("No test cases to validate");
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    struct ValJob {
        idx: usize,
        name: String,
        input_path: PathBuf,
        subtask_id: Option<u32>,
    }

    enum ValResult {
        Ok,
        Failed {
            idx: usize,
            name: String,
            msg: String,
        },
        Error {
            err: String,
        },
    }

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<ValJob>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<ValResult>();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    for (idx, test) in tests.iter().enumerate() {
        let _ = job_tx.send(ValJob {
            idx,
            name: test.name.clone(),
            input_path: test.input_path.clone(),
            subtask_id: test.subtask_id,
        });
    }
    drop(job_tx);

    let val_binary = compiled_val.binary_path.clone();
    let prob_dir = problem_dir.to_path_buf();

    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let val_bin = val_binary.clone();
        let p_dir = prob_dir.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox("auto", false) {
                Ok(s) => s,
                Err(e) => {
                    let _ = res_tx.send(ValResult::Error {
                        err: format!("Sandbox error: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let _ = sandbox.authorize_executable(&val_bin);

            while let Ok(job) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                match crate::author_steps::exec_validate(
                    sandbox.as_ref(),
                    &val_bin,
                    &job.input_path,
                    job.subtask_id,
                    &p_dir,
                    &cancel,
                ) {
                    Ok(_) => {
                        let _ = res_tx.send(ValResult::Ok);
                    }
                    Err(crate::author_steps::StepError::Failed(msg)) => {
                        let _ = res_tx.send(ValResult::Failed {
                            idx: job.idx,
                            name: job.name,
                            msg,
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(crate::author_steps::StepError::Runtime(err)) => {
                        let _ = res_tx.send(ValResult::Error { err });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        handles.push(handle);
    }
    drop(res_tx);

    let pb = if format == OutputFormat::Human && !tests.is_empty() {
        let p = ProgressBar::new(tests.len() as u64);
        p.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} Validating tests [{bar:25.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    let mut first_failure: Option<(usize, String, String)> = None;
    let mut runtime_error: Option<String> = None;
    let mut success_count = 0;

    while let Ok(res) = res_rx.recv() {
        match res {
            ValResult::Ok => {
                success_count += 1;
                if let Some(ref p) = pb {
                    p.inc(1);
                }
            }
            ValResult::Failed { idx, name, msg } => {
                if let Some((f_idx, _, _)) = first_failure {
                    if idx < f_idx {
                        first_failure = Some((idx, name, msg));
                    }
                } else {
                    first_failure = Some((idx, name, msg));
                }
            }
            ValResult::Error { err } => {
                if runtime_error.is_none() {
                    runtime_error = Some(err);
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

    if cancel.is_cancelled() {
        return ExitCode::from(EXIT_INTERRUPTED);
    }

    if let Some(err) = runtime_error {
        eprintln!("{err}");
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, name, msg)) = first_failure {
        if format == OutputFormat::Json {
            let err = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "ValidationError",
                    "test": name,
                    "message": msg
                }
            });
            println!("{}", serde_json::to_string_pretty(&err).unwrap());
        } else {
            eprintln!("Validator failed on test '{name}': {msg}");
        }
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "validated": success_count });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        println!("All {} test case(s) successfully validated", success_count);
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 5. uj ans
// ============================================================================

pub fn run_ans(
    problem_dir: &Path,
    force: bool,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_ans_with_session(problem_dir, force, format, cancel, &session)
}

pub fn run_ans_with_session(
    problem_dir: &Path,
    force: bool,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

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
        Err(code) => return code,
    };

    let sol_path = problem_dir.join(&main_sol.path);
    let compiled = match session.compile(&sol_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile main solution: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to discover tests: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let main_hash = match crate::author_steps::hash_file_streaming(&compiled.binary_path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to read binary for hash: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };
    let limits = cfg
        .to_limits()
        .unwrap_or_else(|_| Limits::new(1000, 256 * 1024 * 1024));

    let topo = uj_engine::topology::Topology::detect(limits.memory_bytes, None);
    let num_workers = topo.slots.len().max(1).min(tests.len().max(1));

    struct AnsJob {
        idx: usize,
        name: String,
        input_path: PathBuf,
        ans_path: PathBuf,
    }

    enum AnsResult {
        Generated,
        Failed {
            idx: usize,
            name: String,
            reason: String,
        },
        Error {
            err: String,
        },
    }

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<AnsJob>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<AnsResult>();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    for (idx, test) in tests.iter().enumerate() {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let in_hash = match crate::author_steps::hash_file_streaming(&test.input_path) {
            Ok(h) => h,
            Err(e) => {
                eprintln!(
                    "Failed to read input file for hash {}: {e}",
                    test.input_path.display()
                );
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        let ans_path = test.input_path.with_extension("ans");
        let cache_meta = problem_dir
            .join(".uj")
            .join("cache")
            .join("ans")
            .join(&main_hash)
            .join(format!("{in_hash}.ans"));

        if !force && ans_path.exists() && cache_meta.exists() {
            continue;
        }

        let _ = job_tx.send(AnsJob {
            idx,
            name: test.name.clone(),
            input_path: test.input_path.clone(),
            ans_path,
        });
    }
    drop(job_tx);

    let sol_binary = compiled.binary_path.clone();
    let prob_dir = problem_dir.to_path_buf();
    let run_limits = limits.clone();

    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let sol_bin = sol_binary.clone();
        let p_dir = prob_dir.clone();
        let limits = run_limits.clone();
        let main_hash_w = main_hash.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox("auto", false) {
                Ok(s) => s,
                Err(e) => {
                    let _ = res_tx.send(AnsResult::Error {
                        err: format!("Sandbox error: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let _ = sandbox.authorize_executable(&sol_bin);

            while let Ok(job) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }

                match crate::author_steps::exec_ans(
                    sandbox.as_ref(),
                    &sol_bin,
                    &main_hash_w,
                    &job.input_path,
                    &job.ans_path,
                    &p_dir,
                    &limits,
                    force,
                    &cancel,
                ) {
                    Ok(_outcome) => {
                        let _ = res_tx.send(AnsResult::Generated);
                    }
                    Err(crate::author_steps::StepError::Failed(reason)) => {
                        let _ = res_tx.send(AnsResult::Failed {
                            idx: job.idx,
                            name: job.name,
                            reason,
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(crate::author_steps::StepError::Runtime(err)) => {
                        let _ = res_tx.send(AnsResult::Error { err });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        handles.push(handle);
    }
    drop(res_tx);

    let pb = if format == OutputFormat::Human && !tests.is_empty() {
        let p = ProgressBar::new(tests.len() as u64);
        p.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} Generating answers [{bar:25.cyan/blue}] {pos}/{len} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    let mut generated_count = 0;
    let mut first_failure: Option<(usize, String, String)> = None;
    let mut runtime_error: Option<String> = None;

    while let Ok(res) = res_rx.recv() {
        match res {
            AnsResult::Generated => {
                generated_count += 1;
                if let Some(ref p) = pb {
                    p.inc(1);
                }
            }
            AnsResult::Failed { idx, name, reason } => {
                if let Some((f_idx, _, _)) = first_failure {
                    if idx < f_idx {
                        first_failure = Some((idx, name, reason));
                    }
                } else {
                    first_failure = Some((idx, name, reason));
                }
            }
            AnsResult::Error { err } => {
                if runtime_error.is_none() {
                    runtime_error = Some(err);
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

    if cancel.is_cancelled() {
        return ExitCode::from(EXIT_INTERRUPTED);
    }

    if let Some(err) = runtime_error {
        eprintln!("{err}");
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, name, reason)) = first_failure {
        eprintln!("Main solution failed on test '{name}': {reason}");
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "answers_generated": generated_count });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        println!("Generated {generated_count} answer file(s)");
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 6. uj verify
// ============================================================================

pub fn run_verify(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_verify_with_session(problem_dir, cli, format, cancel, &session)
}

/// Pure function to verify whether a submission judge report satisfies the expected SolutionTag and subtasks.
pub fn check_solution_tag(
    sol: &SolutionConfig,
    report: &SubmissionJudgeReport,
    cfg: &ProblemConfig,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut matches_tag = true;

    match sol.tag {
        SolutionTag::Main | SolutionTag::Accepted => {
            if report.verdict != Verdict::Accepted {
                matches_tag = false;
            }
        }
        SolutionTag::WrongAnswer => {
            let has_wa = report
                .tests
                .iter()
                .any(|t| t.verdict == Verdict::WrongAnswer);
            let has_fatal = report.tests.iter().any(|t| {
                matches!(
                    t.verdict,
                    Verdict::TimeLimitExceeded
                        | Verdict::MemoryLimitExceeded
                        | Verdict::RuntimeError
                        | Verdict::OutputLimitExceeded
                        | Verdict::RestrictedFunction
                        | Verdict::JudgeFailed
                )
            });
            if !has_wa || has_fatal {
                matches_tag = false;
            }
        }
        SolutionTag::TimeLimit => {
            let has_tle = report
                .tests
                .iter()
                .any(|t| t.verdict == Verdict::TimeLimitExceeded);
            let has_other = report.tests.iter().any(|t| {
                matches!(
                    t.verdict,
                    Verdict::WrongAnswer
                        | Verdict::MemoryLimitExceeded
                        | Verdict::RuntimeError
                        | Verdict::OutputLimitExceeded
                        | Verdict::RestrictedFunction
                        | Verdict::JudgeFailed
                )
            });
            if !has_tle || has_other {
                matches_tag = false;
            }
        }
        SolutionTag::MemoryLimit => {
            let has_mle = report
                .tests
                .iter()
                .any(|t| t.verdict == Verdict::MemoryLimitExceeded);
            let has_other = report.tests.iter().any(|t| {
                matches!(
                    t.verdict,
                    Verdict::WrongAnswer
                        | Verdict::TimeLimitExceeded
                        | Verdict::RuntimeError
                        | Verdict::OutputLimitExceeded
                        | Verdict::RestrictedFunction
                        | Verdict::JudgeFailed
                )
            });
            if !has_mle || has_other {
                matches_tag = false;
            }
        }
        SolutionTag::RuntimeError => {
            let has_re = report
                .tests
                .iter()
                .any(|t| t.verdict == Verdict::RuntimeError);
            let has_other = report.tests.iter().any(|t| {
                matches!(
                    t.verdict,
                    Verdict::WrongAnswer
                        | Verdict::TimeLimitExceeded
                        | Verdict::MemoryLimitExceeded
                        | Verdict::OutputLimitExceeded
                        | Verdict::RestrictedFunction
                        | Verdict::JudgeFailed
                )
            });
            if !has_re || has_other {
                matches_tag = false;
            }
        }
        SolutionTag::Rejected => {
            if report.verdict == Verdict::Accepted || report.verdict == Verdict::JudgeFailed {
                matches_tag = false;
            }
        }
        SolutionTag::Partial => {
            let max_score = cfg.max_score();
            if report.score <= 0.0 || report.score >= max_score {
                matches_tag = false;
            }
        }
    }

    if !matches_tag {
        let msg = report
            .tests
            .iter()
            .find_map(|t| t.message.as_ref())
            .cloned()
            .unwrap_or_default();
        failures.push(format!(
            "{} expected tag '{:?}' but got verdict '{:?}' (score: {}, msg: {msg})",
            sol.path.display(),
            sol.tag,
            report.verdict,
            report.score
        ));
    }

    // Subtask expectations check
    if let Some(ref expect_subtasks) = sol.expect_subtasks {
        for (st_id, expected_v) in expect_subtasks {
            let actual_v = report
                .subtasks
                .iter()
                .find(|s| s.id == *st_id)
                .map(|s| s.verdict);
            if actual_v != Some(*expected_v) {
                failures.push(format!(
                    "{} subtask {st_id} expected '{expected_v:?}' but got '{actual_v:?}'",
                    sol.path.display()
                ));
            }
        }
    }

    failures
}

pub fn run_verify_with_session(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to discover tests: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let sandbox_mode_str = match cli.sandbox {
        SandboxMode::Auto => "auto",
        SandboxMode::Full => "full",
        SandboxMode::Degraded => "degraded",
        SandboxMode::None => "none",
    };

    let total_sols = cfg.solutions.len();
    if total_sols == 0 {
        if format == OutputFormat::Json {
            let res = serde_json::json!({ "ok": true, "verified": 0 });
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        } else if format == OutputFormat::Human {
            println!("All 0 solution(s) passed verification!");
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    // Phase A: Parallel compilation & context preparation
    let problem_ctx = match ProblemContext::prepare(0, problem_dir, &cfg, session) {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            eprintln!("Problem setup error: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    enum PreparedSol {
        Ready(usize, Arc<SubmissionContext>),
        CompileError(usize, String),
        PrepareError(usize, String),
    }

    let detected_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let compile_workers = (detected_cpus / 2).clamp(1, 8);
    let (task_tx, task_rx) = crossbeam_channel::unbounded::<(usize, SolutionConfig)>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<PreparedSol>();

    for _ in 0..compile_workers {
        let rx = task_rx.clone();
        let tx = res_tx.clone();
        let cancel_w = cancel.clone();
        let session_w = session.clone();
        let p_ctx = Arc::clone(&problem_ctx);
        let tests_w = tests.clone();
        let problem_dir_w = problem_dir.to_path_buf();
        let no_cache = cli.no_cache;
        let fail_fast = cli.fail_fast;

        std::thread::spawn(move || {
            while let Ok((idx, sol)) = rx.recv() {
                if cancel_w.is_cancelled() {
                    break;
                }
                let sol_path = problem_dir_w.join(&sol.path);
                let compiled = match session_w.compile(&sol_path, no_cache) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(PreparedSol::CompileError(idx, e.to_string()));
                        continue;
                    }
                };

                let sol_name = sol.path.file_name().unwrap_or_default().to_string_lossy();
                match SubmissionContext::prepare(
                    idx,
                    &sol_name,
                    Arc::clone(&p_ctx),
                    &compiled,
                    tests_w.clone(),
                    fail_fast,
                ) {
                    Ok(sub_ctx) => {
                        let _ = tx.send(PreparedSol::Ready(idx, Arc::new(sub_ctx)));
                    }
                    Err(e) => {
                        let _ = tx.send(PreparedSol::PrepareError(idx, e.to_string()));
                    }
                }
            }
        });
    }
    drop(res_tx);

    for (idx, sol) in cfg.solutions.iter().enumerate() {
        let _ = task_tx.send((idx, sol.clone()));
    }
    drop(task_tx);

    let mut ready_map = std::collections::HashMap::new();
    let mut compile_errors = std::collections::HashMap::new();
    let mut prepare_errors = std::collections::HashMap::new();

    while let Ok(res) = res_rx.recv() {
        match res {
            PreparedSol::Ready(idx, ctx) => {
                ready_map.insert(idx, ctx);
            }
            PreparedSol::CompileError(idx, err) => {
                compile_errors.insert(idx, err);
            }
            PreparedSol::PrepareError(idx, err) => {
                prepare_errors.insert(idx, err);
            }
        }
    }

    if cancel.is_cancelled() {
        return ExitCode::from(EXIT_INTERRUPTED);
    }

    // Phase B / C: JudgePool evaluation
    let topology = uj_engine::topology::Topology::detect(0, cli.jobs);
    let slots = topology.slots.len();
    let window = std::cmp::max(2, slots / 2);
    let memory_budget = uj_engine::topology::Topology::memory_budget_bytes();

    let (ptx, prx) = crossbeam_channel::unbounded::<PoolEvent>();
    let pool_events_opt = if cli.tui { Some(ptx.clone()) } else { None };

    let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
    let sub_metas: Vec<SubmissionMeta> = cfg
        .solutions
        .iter()
        .enumerate()
        .map(|(idx, sol)| {
            let sol_disp = sol.path.display().to_string();
            SubmissionMeta {
                id: idx,
                problem: cfg.name.clone(),
                name: sol_disp,
                test_names: test_names.clone(),
            }
        })
        .collect();

    if let Some(ref tx) = pool_events_opt {
        for idx in compile_errors.keys() {
            let sol = &cfg.solutions[*idx];
            let fail_rep = SubmissionJudgeReport {
                schema_version: 1,
                problem: cfg.name.clone(),
                submission: sol.path.display().to_string(),
                verdict: Verdict::CompileError,
                score: 0.0,
                max_time_ms: 0,
                max_memory_kb: 0,
                subtasks: vec![],
                tests: vec![],
            };
            let _ = tx.send(PoolEvent {
                submission: SubmissionRef {
                    id: *idx,
                    problem: cfg.name.clone(),
                    name: sol.path.display().to_string(),
                },
                inner: JudgeEvent::SubmissionDone(fail_rep),
            });
        }
        for idx in prepare_errors.keys() {
            let sol = &cfg.solutions[*idx];
            let fail_rep = SubmissionJudgeReport {
                schema_version: 1,
                problem: cfg.name.clone(),
                submission: sol.path.display().to_string(),
                verdict: Verdict::JudgeFailed,
                score: 0.0,
                max_time_ms: 0,
                max_memory_kb: 0,
                subtasks: vec![],
                tests: vec![],
            };
            let _ = tx.send(PoolEvent {
                submission: SubmissionRef {
                    id: *idx,
                    problem: cfg.name.clone(),
                    name: sol.path.display().to_string(),
                },
                inner: JudgeEvent::SubmissionDone(fail_rep),
            });
        }
    }

    let tui_handle = if cli.tui {
        let slot_cpus: Vec<Option<usize>> = topology.slots.iter().map(|s| s.cpu_core).collect();
        let title = format!("Verify: {}", cfg.name);
        let cancel_tui = cancel.clone();
        let rx = prx.clone();
        Some(std::thread::spawn(move || {
            let _ = run_multi_tui(&title, &sub_metas, slots, slot_cpus, rx, cancel_tui, true);
        }))
    } else {
        None
    };

    let pool = JudgePool::new(PoolOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        slots,
        window,
        memory_budget,
        pool_events: pool_events_opt,
    });

    let show_progress = format == OutputFormat::Human && !cli.quiet && !cli.tui;
    let mp = if show_progress {
        Some(MultiProgress::new())
    } else {
        None
    };
    let mut event_threads = Vec::new();
    let mut pool_subs = Vec::new();
    let mut sub_idx_to_sol_idx = Vec::new();
    let total_cnt = tests.len() as u64;

    for (idx, sol) in cfg.solutions.iter().enumerate() {
        if let Some(sub_ctx) = ready_map.get(&idx) {
            let is_positive = matches!(sol.tag, SolutionTag::Main | SolutionTag::Accepted);
            let (tx, rx) = if show_progress {
                let (tx, rx) = crossbeam_channel::unbounded::<JudgeEvent>();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            if let (Some(mp_ref), Some(rx)) = (&mp, rx) {
                let pb = mp_ref.add(ProgressBar::new(total_cnt));
                let sol_num = idx + 1;
                let sol_disp = sol.path.display().to_string();
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(&format!(
                            "{{spinner:.green}} [{sol_num}/{total_sols}] {sol_disp} ({{msg}}) [{{bar:25.cyan/blue}}] {{pos}}/{{len}}"
                        ))
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("=>-"),
                );
                pb.set_message("testing...");
                let handle = std::thread::spawn(move || {
                    for ev in rx {
                        match ev {
                            JudgeEvent::TestDone(ref res) => {
                                pb.inc(1);
                                let mem_str = if res.memory_kb > 1024 {
                                    format!("{:.1}M", res.memory_kb as f64 / 1024.0)
                                } else {
                                    format!("{}K", res.memory_kb)
                                };
                                pb.set_message(format!(
                                    "{}: {} ({}ms, {mem_str})",
                                    res.id,
                                    res.verdict.short_code(),
                                    res.time_ms
                                ));
                            }
                            JudgeEvent::SubmissionDone(_) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    pb.finish_and_clear();
                });
                event_threads.push(handle);
            }

            pool_subs.push(PoolSubmission {
                ctx: Arc::clone(sub_ctx),
                events: tx,
                precise: cli.precise && is_positive,
            });
            sub_idx_to_sol_idx.push(idx);
        }
    }

    let pool_reports = pool.run(pool_subs, cancel);
    drop(pool);
    drop(ptx);

    for h in event_threads {
        let _ = h.join();
    }
    if let Some(h) = tui_handle {
        let _ = h.join();
    }

    if let Some(mp_ref) = mp {
        let _ = mp_ref.clear();
    }

    let mut reports_by_sol_idx = std::collections::HashMap::new();
    for (sub_res, sol_idx) in pool_reports.into_iter().zip(sub_idx_to_sol_idx) {
        reports_by_sol_idx.insert(sol_idx, sub_res);
    }

    let mut verification_failures = Vec::new();

    // Verification result reporting in deterministic cfg.solutions order
    for (idx, sol) in cfg.solutions.iter().enumerate() {
        let sol_num = idx + 1;
        let tag_name = format!("{:?}", sol.tag);

        if let Some(err) = compile_errors.get(&idx) {
            if format == OutputFormat::Human {
                eprintln!(
                    "[{sol_num}/{total_sols}] {} ({tag_name}) -> COMPILATION FAILED: {err}",
                    sol.path.display()
                );
            }
            verification_failures
                .push(format!("{}: compilation failed: {err}", sol.path.display()));
            continue;
        }

        if let Some(err) = prepare_errors.get(&idx) {
            if format == OutputFormat::Human {
                eprintln!(
                    "[{sol_num}/{total_sols}] {} ({tag_name}) -> PREPARE FAILED: {err}",
                    sol.path.display()
                );
            }
            verification_failures.push(format!("{}: prepare failed: {err}", sol.path.display()));
            continue;
        }

        match reports_by_sol_idx.remove(&idx) {
            Some(Ok(report)) => {
                let sol_failures = check_solution_tag(sol, &report, &cfg);
                let matches_tag = sol_failures.is_empty();
                verification_failures.extend(sol_failures);

                if format == OutputFormat::Human {
                    let max_time = report.tests.iter().map(|t| t.time_ms).max().unwrap_or(0);
                    let mem_str = if report.max_memory_kb > 1024 {
                        format!("{:.1} MiB", report.max_memory_kb as f64 / 1024.0)
                    } else {
                        format!("{} KiB", report.max_memory_kb)
                    };
                    if matches_tag {
                        println!(
                            "[{sol_num}/{total_sols}] {} ({tag_name}) -> OK ({:?}, {max_time} ms, {mem_str})",
                            sol.path.display(),
                            report.verdict
                        );
                    } else {
                        println!(
                            "[{sol_num}/{total_sols}] {} ({tag_name}) -> FAILED (got {:?}, {max_time} ms, {mem_str})",
                            sol.path.display(),
                            report.verdict
                        );
                    }
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
            Some(Err(e)) => {
                if format == OutputFormat::Human {
                    eprintln!(
                        "[{sol_num}/{total_sols}] {} ({tag_name}) -> JUDGE FAILED: {e}",
                        sol.path.display()
                    );
                }
                verification_failures.push(format!("{}: judging failed: {e}", sol.path.display()));
            }
            None => {
                verification_failures
                    .push(format!("{}: missing evaluation result", sol.path.display()));
            }
        }
    }

    if !verification_failures.is_empty() {
        if format == OutputFormat::Json {
            let err = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "VerifyMismatch",
                    "failures": verification_failures
                }
            });
            println!("{}", serde_json::to_string_pretty(&err).unwrap());
        } else {
            eprintln!(
                "Verification failed for {} solution(s):",
                verification_failures.len()
            );
            for f in &verification_failures {
                eprintln!("  - {f}");
            }
        }
        return ExitCode::from(EXIT_VERIFY_MISMATCH);
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "verified": cfg.solutions.len() });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        println!(
            "All {} solution(s) passed verification!",
            cfg.solutions.len()
        );
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 7. uj tl
// ============================================================================

pub fn run_tl(
    problem_dir: &Path,
    apply: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_tl_with_session(problem_dir, apply, cli, format, cancel, &session)
}

pub fn run_tl_with_session(
    problem_dir: &Path,
    apply: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read problem.toml: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to discover tests: {e}");
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let target_solutions: Vec<_> = cfg
        .solutions
        .iter()
        .filter(|s| s.tag == SolutionTag::Main || s.tag == SolutionTag::Accepted)
        .collect();

    if target_solutions.is_empty() {
        eprintln!("No main or accepted solutions found for calibration");
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = match cli.sandbox {
        SandboxMode::Auto => "auto",
        SandboxMode::Full => "full",
        SandboxMode::Degraded => "degraded",
        SandboxMode::None => "none",
    };

    let options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: false,
        precise: false,
    };
    let scheduler = Scheduler::with_session(options, session.clone());

    let mut t_main = 0u64;
    let mut t_slowest_ac = 0u64;
    let mut peak_mem_kb = 0u64;

    let total_sols = target_solutions.len();
    for (sol_idx, sol) in target_solutions.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }
        let sol_path = problem_dir.join(&sol.path);
        let compiled = match session.compile(&sol_path, false) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to compile solution: {e}");
                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        };

        let sol_name = sol.path.file_name().unwrap_or_default().to_string_lossy();
        let sol_disp = sol.path.display().to_string();
        let sol_num = sol_idx + 1;

        // Run 3 times in exclusive mode (jobs = 1)
        for run_idx in 0..3 {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }

            let round_num = run_idx + 1;
            let sub_name = format!("{sol_disp} [calibrating {round_num}/3]");

            let (event_sender, event_thread) = if cli.tui {
                let (tx, rx) = crossbeam_channel::unbounded::<uj_report::result::JudgeEvent>();
                let prob_name = cfg.name.clone();
                let cancel_clone = cancel.clone();
                let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
                let cur_sub = sub_name.clone();
                let handle = std::thread::spawn(move || {
                    let _ = uj_tui::run_tui(
                        &prob_name,
                        &cur_sub,
                        &test_names,
                        1, // 1 exclusive core for calibration
                        rx,
                        cancel_clone,
                        true,
                    );
                });
                (Some(tx), Some(handle))
            } else if format == OutputFormat::Human && !cli.quiet {
                let (tx, rx) = crossbeam_channel::unbounded::<uj_report::result::JudgeEvent>();
                let total_cnt = tests.len() as u64;
                let cur_sub = sub_name.clone();
                let handle = std::thread::spawn(move || {
                    let pb = ProgressBar::new(total_cnt);
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template(&format!(
                                "{{spinner:.green}} [{sol_num}/{total_sols}] {cur_sub} ({{msg}}) [{{bar:25.cyan/blue}}] {{pos}}/{{len}}"
                            ))
                            .unwrap_or_else(|_| ProgressStyle::default_bar())
                            .progress_chars("=>-"),
                    );
                    pb.set_message("calibrating...");
                    for ev in rx {
                        match ev {
                            uj_report::result::JudgeEvent::TestDone(ref res) => {
                                pb.inc(1);
                                let mem_str = if res.memory_kb > 1024 {
                                    format!("{:.1}M", res.memory_kb as f64 / 1024.0)
                                } else {
                                    format!("{}K", res.memory_kb)
                                };
                                pb.set_message(format!(
                                    "{}: {} ({}ms, {mem_str})",
                                    res.id,
                                    res.verdict.short_code(),
                                    res.time_ms
                                ));
                            }
                            uj_report::result::JudgeEvent::SubmissionDone(_) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    pb.finish_and_clear();
                });
                (Some(tx), Some(handle))
            } else {
                (None, None)
            };

            match scheduler.judge_submission(
                problem_dir,
                &cfg,
                &compiled,
                &sol_name,
                tests.clone(),
                Some(1),
                event_sender,
                cancel,
            ) {
                Ok(report) => {
                    if let Some(h) = event_thread {
                        let _ = h.join();
                    }
                    if !report.verdict.is_accepted() {
                        if format == OutputFormat::Json {
                            let err = serde_json::json!({
                                "ok": false,
                                "error": {
                                    "kind": "CalibrationError",
                                    "message": format!("Solution '{}' failed with verdict {:?}", sol_name, report.verdict)
                                }
                            });
                            println!("{}", serde_json::to_string_pretty(&err).unwrap());
                        } else if format == OutputFormat::Human {
                            eprintln!(
                                "Solution '{}' failed with verdict {:?}",
                                sol_name, report.verdict
                            );
                        }
                        return ExitCode::from(EXIT_NON_AC);
                    }
                    let max_ms = report.max_time_ms;
                    peak_mem_kb = peak_mem_kb.max(report.max_memory_kb);
                    if sol.tag == SolutionTag::Main {
                        t_main = t_main.max(max_ms);
                    }
                    t_slowest_ac = t_slowest_ac.max(max_ms);
                }
                Err(e) => {
                    if let Some(h) = event_thread {
                        let _ = h.join();
                    }
                    if format == OutputFormat::Json {
                        let err = serde_json::json!({
                            "ok": false,
                            "error": {
                                "kind": "JudgeFailed",
                                "message": format!("Calibration judge failed: {e}")
                            }
                        });
                        println!("{}", serde_json::to_string_pretty(&err).unwrap());
                    } else if format == OutputFormat::Human {
                        eprintln!("Calibration judge failed: {e}");
                    }
                    return ExitCode::from(EXIT_JUDGE_FAILED);
                }
            }
        }
    }

    let raw_tl = (2.0 * t_main as f64).max(1.5 * t_slowest_ac as f64);
    let mut rec_tl_ms = ((raw_tl / 100.0).ceil() as u64) * 100;
    if rec_tl_ms < 100 {
        rec_tl_ms = 100;
    }

    let formatted_tl = if rec_tl_ms >= 1000 && rec_tl_ms.is_multiple_of(1000) {
        format!("{}s", rec_tl_ms / 1000)
    } else if rec_tl_ms >= 1000 {
        format!("{:.1}s", rec_tl_ms as f64 / 1000.0)
    } else {
        format!("{rec_tl_ms}ms")
    };

    if apply {
        let pattern = regex::Regex::new(r#"(?m)^\s*time\s*=\s*"[^"]*""#).unwrap();
        let updated = pattern.replace(&content, format!(r#"time = "{formatted_tl}""#));
        if let Err(e) = fs::write(&config_path, updated.as_bytes()) {
            if format == OutputFormat::Json {
                let err = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "ConfigWriteError",
                        "message": format!("Failed to write updated problem.toml: {e}")
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err).unwrap());
            } else if format == OutputFormat::Human {
                eprintln!("Failed to write updated problem.toml: {e}");
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({
            "ok": true,
            "t_main_ms": t_main,
            "t_slowest_ac_ms": t_slowest_ac,
            "peak_memory_kb": peak_mem_kb,
            "recommended_tl_ms": rec_tl_ms,
            "recommended_tl": formatted_tl,
            "applied": apply
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Human {
        let mem_str = if peak_mem_kb > 1024 {
            format!("{:.1} MiB", peak_mem_kb as f64 / 1024.0)
        } else {
            format!("{} KiB", peak_mem_kb)
        };
        println!("=== CPU Time Limit Calibration ===");
        println!("  Main solution peak CPU:       {t_main} ms");
        println!("  Slowest accepted peak CPU:    {t_slowest_ac} ms");
        println!("  Peak memory observed:         {mem_str}");
        println!("  Recommended Time Limit:       {formatted_tl} ({rec_tl_ms} ms)");
        if apply {
            println!("  [Applied] Updated limits.time in problem.toml");
        }
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 8. uj stress
// ============================================================================

#[allow(clippy::too_many_arguments)]
pub fn run_stress(
    problem_dir: &Path,
    gen_src: &str,
    sol_src: &str,
    brute_src: &str,
    count: usize,
    seed_opt: Option<u64>,
    time_budget_str: &str,
    _cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let seconds = time_budget_str
        .trim_end_matches('s')
        .parse::<u64>()
        .unwrap_or(60);
    let budget = Duration::from_secs(seconds);
    let start_time = Instant::now();

    let gen_path = problem_dir.join(gen_src);
    let sol_path = problem_dir.join(sol_src);
    let brute_path = problem_dir.join(brute_src);

    let compile_session = CompileSession::new();
    let compiled_gen = match compile_session.compile(&gen_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile generator: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };
    let compiled_sol = match compile_session.compile(&sol_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile target solution: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };
    let compiled_brute = match compile_session.compile(&brute_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile brute oracle: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };

    let sandbox = match create_sandbox("auto", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Sandbox error: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };
    let _ = sandbox.authorize_executable(&compiled_gen.binary_path);
    let _ = sandbox.authorize_executable(&compiled_sol.binary_path);
    let _ = sandbox.authorize_executable(&compiled_brute.binary_path);

    let wcmp = BuiltinCheckerType::Wcmp;
    let seed_start = seed_opt.unwrap_or(1);
    let seed_counter = Arc::new(AtomicU64::new(seed_start));

    let mut iterations = 0;
    while iterations < count && start_time.elapsed() < budget {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let seed = seed_counter.fetch_add(1, Ordering::SeqCst);
        iterations += 1;

        // 1. Generate input
        let limits = Limits::new(5000, 512 * 1024 * 1024);
        let spec_gen = RunSpec {
            argv: vec![
                compiled_gen.binary_path.as_os_str().to_os_string(),
                std::ffi::OsString::from(format!("{seed}")),
            ],
            env: vec![],
            work_dir: problem_dir.to_path_buf(),
            ro_binds: vec![],
            stdin: Stdio::Null,
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits: limits.clone(),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: true,
        };

        let gen_res = match sandbox.run(&spec_gen, cancel) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Generator failed: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        if !gen_res.status.is_success() || gen_res.killed_by.is_some() || gen_res.oom_killed {
            let stderr_msg = String::from_utf8_lossy(&gen_res.stderr_tail);
            let reason = format!(
                "Generator failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                gen_res.status,
                gen_res.killed_by,
                gen_res.oom_killed,
                stderr_msg.trim()
            );
            if format == OutputFormat::Json {
                let res = serde_json::json!({
                    "ok": false,
                    "seed": seed,
                    "error": {
                        "kind": "GeneratorFailed",
                        "message": reason
                    }
                });
                println!("{}", serde_json::to_string_pretty(&res).unwrap());
            } else if format == OutputFormat::Human {
                eprintln!("{reason}");
            }
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        let in_bytes = gen_res.stdout.map(|o| o.bytes).unwrap_or_default();

        // Write temp input to sandbox scratch
        let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
        let in_tmp = scratch_dir.join(format!("stress_{seed}.in"));
        let _ = fs::write(&in_tmp, &in_bytes);

        // 2. Run target solution
        let spec_sol = RunSpec {
            argv: vec![compiled_sol.binary_path.as_os_str().to_os_string()],
            env: vec![],
            work_dir: scratch_dir.clone(),
            ro_binds: vec![],
            stdin: Stdio::File(in_tmp.clone()),
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits: limits.clone(),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: false,
        };

        let sol_res = match sandbox.run(&spec_sol, cancel) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&in_tmp);
                eprintln!("Solution execution failed: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        if !sol_res.status.is_success() || sol_res.killed_by.is_some() || sol_res.oom_killed {
            let stress_dir = problem_dir.join("stress");
            let _ = fs::create_dir_all(&stress_dir);
            let fail_in = stress_dir.join(format!("fail-{seed}.in"));
            let fail_sol = stress_dir.join(format!("fail-{seed}.sol.out"));
            let _ = fs::write(&fail_in, &in_bytes);
            let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();
            let _ = fs::write(&fail_sol, &sol_bytes);
            let reason = format!(
                "Solution failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                sol_res.status,
                sol_res.killed_by,
                sol_res.oom_killed,
                String::from_utf8_lossy(&sol_res.stderr_tail).trim()
            );
            if format == OutputFormat::Json {
                let res = serde_json::json!({
                    "ok": false,
                    "seed": seed,
                    "diff": reason,
                    "saved_to": stress_dir.to_string_lossy()
                });
                println!("{}", serde_json::to_string_pretty(&res).unwrap());
            } else if format == OutputFormat::Human {
                println!("=== Stress Test Failure Found on Seed {seed} ===");
                println!("  Message:    {reason}");
                println!("  Input:      {}", fail_in.display());
                println!(
                    "  Repro:      uj run -- {} < {}",
                    compiled_sol.binary_path.display(),
                    fail_in.display()
                );
            }
            let _ = fs::remove_file(&in_tmp);
            return ExitCode::from(EXIT_NON_AC);
        }

        // 3. Run brute oracle
        let spec_brute = RunSpec {
            argv: vec![compiled_brute.binary_path.as_os_str().to_os_string()],
            env: vec![],
            work_dir: scratch_dir,
            ro_binds: vec![],
            stdin: Stdio::File(in_tmp.clone()),
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits,
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: false,
        };

        let brute_res = match sandbox.run(&spec_brute, cancel) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&in_tmp);
                eprintln!("Brute execution failed: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        let _ = fs::remove_file(&in_tmp);

        if !brute_res.status.is_success() || brute_res.killed_by.is_some() || brute_res.oom_killed {
            let stderr_msg = String::from_utf8_lossy(&brute_res.stderr_tail);
            let reason = format!(
                "Brute oracle failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                brute_res.status,
                brute_res.killed_by,
                brute_res.oom_killed,
                stderr_msg.trim()
            );
            if format == OutputFormat::Json {
                let res = serde_json::json!({
                    "ok": false,
                    "seed": seed,
                    "error": {
                        "kind": "BruteFailed",
                        "message": reason
                    }
                });
                println!("{}", serde_json::to_string_pretty(&res).unwrap());
            } else if format == OutputFormat::Human {
                eprintln!("{reason}");
            }
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();
        let brute_bytes = brute_res.stdout.map(|o| o.bytes).unwrap_or_default();

        let check = wcmp.check(&sol_bytes, &brute_bytes);
        if !check.verdict.is_accepted() {
            // Mismatch found!
            let stress_dir = problem_dir.join("stress");
            let _ = fs::create_dir_all(&stress_dir);
            let fail_in = stress_dir.join(format!("fail-{seed}.in"));
            let fail_sol = stress_dir.join(format!("fail-{seed}.sol.out"));
            let fail_brute = stress_dir.join(format!("fail-{seed}.brute.out"));

            let _ = fs::write(&fail_in, &in_bytes);
            let _ = fs::write(&fail_sol, &sol_bytes);
            let _ = fs::write(&fail_brute, &brute_bytes);

            if format == OutputFormat::Json {
                let res = serde_json::json!({
                    "ok": false,
                    "seed": seed,
                    "diff": check.message,
                    "saved_to": stress_dir.to_string_lossy()
                });
                println!("{}", serde_json::to_string_pretty(&res).unwrap());
            } else {
                println!("=== Stress Test Mismatch Found on Seed {seed} ===");
                println!(
                    "  Message:    {}",
                    check.message.as_deref().unwrap_or("Output mismatch")
                );
                println!("  Input:      {}", fail_in.display());
                println!("  Target:     {}", fail_sol.display());
                println!("  Expected:   {}", fail_brute.display());
                println!(
                    "  Repro:      uj run -- {} < {}",
                    compiled_sol.binary_path.display(),
                    fail_in.display()
                );
            }
            return ExitCode::from(EXIT_NON_AC);
        }
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({
            "ok": true,
            "iterations": iterations,
            "duration_ms": start_time.elapsed().as_millis()
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
        println!(
            "Stress test completed: {} iterations verified with no mismatches ({:?})",
            iterations,
            start_time.elapsed()
        );
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 9. uj pipeline
// ============================================================================

pub fn run_pipeline(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    let sub_format = match format {
        OutputFormat::Json => OutputFormat::Silent,
        other => other,
    };

    struct PipelineRunner<'a> {
        step_records: Vec<serde_json::Value>,
        failed_step: Option<String>,
        final_code: ExitCode,
        cancel: &'a CancelToken,
        format: OutputFormat,
    }

    impl<'a> PipelineRunner<'a> {
        fn new(cancel: &'a CancelToken, format: OutputFormat) -> Self {
            Self {
                step_records: Vec::new(),
                failed_step: None,
                final_code: ExitCode::from(EXIT_SUCCESS),
                cancel,
                format,
            }
        }

        fn step(&mut self, name: &'static str, run_fn: impl FnOnce() -> ExitCode) -> bool {
            if self.cancel.is_cancelled() {
                self.final_code = ExitCode::from(EXIT_INTERRUPTED);
                return false;
            }
            if self.format == OutputFormat::Human {
                println!("==> Step: uj {name}");
            } else if self.format == OutputFormat::Ndjson {
                println!(
                    "{}",
                    serde_json::json!({ "event": "step_started", "step": name })
                );
            }

            let code = run_fn();
            let success = code == ExitCode::from(EXIT_SUCCESS);
            self.step_records.push(serde_json::json!({
                "name": name,
                "status": if success { "success" } else { "failed" }
            }));

            if self.format == OutputFormat::Ndjson {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "step_finished",
                        "step": name,
                        "status": if success { "success" } else { "failed" }
                    })
                );
            }

            if !success {
                self.failed_step = Some(name.to_string());
                self.final_code = code;
                if self.format == OutputFormat::Human {
                    eprintln!("Pipeline stopped: step 'uj {name}' failed with status {code:?}");
                }
                return false;
            }
            true
        }

        fn record_step(&mut self, name: &'static str, status: &'static str) {
            self.step_records.push(serde_json::json!({
                "name": name,
                "status": status,
            }));
        }
    }

    let mut runner = PipelineRunner::new(cancel, format);

    let use_staged = std::env::var("UJ_PIPELINE_STAGED").as_deref() == Ok("1");
    if use_staged {
        if runner.step("build", || {
            run_build_with_session(problem_dir, &[], cli.no_cache, sub_format, &session)
        }) && runner.step("gen", || {
            run_gen_with_session(problem_dir, false, false, sub_format, cancel, &session)
        }) && runner.step("validate", || {
            run_validate_with_session(problem_dir, sub_format, cancel, &session)
        }) && runner.step("ans", || {
            run_ans_with_session(problem_dir, false, sub_format, cancel, &session)
        }) && runner.step("verify", || {
            run_verify_with_session(problem_dir, cli, sub_format, cancel, &session)
        }) {
            runner.step("tl", || {
                run_tl_with_session(problem_dir, false, cli, sub_format, cancel, &session)
            });
        }
    } else if runner.step("build", || {
        run_build_with_session(problem_dir, &[], cli.no_cache, sub_format, &session)
    }) {
        if format == OutputFormat::Ndjson {
            println!(
                "{}",
                serde_json::json!({ "event": "step_started", "step": "gen" })
            );
            println!(
                "{}",
                serde_json::json!({ "event": "step_started", "step": "validate" })
            );
            println!(
                "{}",
                serde_json::json!({ "event": "step_started", "step": "ans" })
            );
        } else if format == OutputFormat::Human && !cli.tui {
            println!("==> Step: uj pipeline (gen + validate + ans)");
        }

        let outcome =
            crate::author_steps::run_case_stages(problem_dir, cli, sub_format, cancel, &session);

        match outcome {
            crate::author_steps::CaseStagesOutcome::Success { .. } => {
                runner.record_step("gen", "success");
                runner.record_step("validate", "success");
                runner.record_step("ans", "success");
                if format == OutputFormat::Ndjson {
                    println!(
                        "{}",
                        serde_json::json!({ "event": "step_finished", "step": "gen", "status": "success" })
                    );
                    println!(
                        "{}",
                        serde_json::json!({ "event": "step_finished", "step": "validate", "status": "success" })
                    );
                    println!(
                        "{}",
                        serde_json::json!({ "event": "step_finished", "step": "ans", "status": "success" })
                    );
                }

                if runner.step("verify", || {
                    run_verify_with_session(problem_dir, cli, sub_format, cancel, &session)
                }) {
                    runner.step("tl", || {
                        run_tl_with_session(problem_dir, false, cli, sub_format, cancel, &session)
                    });
                }
            }
            crate::author_steps::CaseStagesOutcome::Failed {
                stage,
                message,
                exit_code,
                ..
            } => {
                runner.final_code = exit_code;
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.failed_step = Some("gen".to_string());
                        runner.record_step("gen", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "failed" })
                            );
                        }
                        if format == OutputFormat::Human {
                            eprintln!("{message}");
                            eprintln!(
                                "Pipeline stopped: step 'uj gen' failed with status {:?}",
                                runner.final_code
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.failed_step = Some("validate".to_string());
                        runner.record_step("gen", "aborted");
                        runner.record_step("validate", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "failed" })
                            );
                        }
                        if format == OutputFormat::Human {
                            eprintln!("{message}");
                            eprintln!(
                                "Pipeline stopped: step 'uj validate' failed with status {:?}",
                                runner.final_code
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.failed_step = Some("ans".to_string());
                        runner.record_step("gen", "aborted");
                        runner.record_step("validate", "aborted");
                        runner.record_step("ans", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "ans", "status": "failed" })
                            );
                        }
                        if format == OutputFormat::Human {
                            eprintln!("{message}");
                            eprintln!(
                                "Pipeline stopped: step 'uj ans' failed with status {:?}",
                                runner.final_code
                            );
                        }
                    }
                }
            }
            crate::author_steps::CaseStagesOutcome::RuntimeError { stage, message } => {
                runner.final_code = ExitCode::from(EXIT_JUDGE_FAILED);
                let s_name = stage.name();
                runner.failed_step = Some(s_name.to_string());
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.record_step("gen", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "failed" })
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.record_step("gen", "aborted");
                        runner.record_step("validate", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "failed" })
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.record_step("gen", "aborted");
                        runner.record_step("validate", "aborted");
                        runner.record_step("ans", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "aborted" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "ans", "status": "failed" })
                            );
                        }
                    }
                }
                if format == OutputFormat::Human {
                    eprintln!("{message}");
                    eprintln!(
                        "Pipeline stopped: step 'uj {s_name}' failed with status {:?}",
                        runner.final_code
                    );
                }
            }
            crate::author_steps::CaseStagesOutcome::ConfigOrCompileError { stage, exit_code } => {
                runner.final_code = exit_code;
                let s_name = stage.name();
                runner.failed_step = Some(s_name.to_string());
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.record_step("gen", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "failed" })
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.record_step("gen", "success");
                        runner.record_step("validate", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "success" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "failed" })
                            );
                        }
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.record_step("gen", "success");
                        runner.record_step("validate", "success");
                        runner.record_step("ans", "failed");
                        if format == OutputFormat::Ndjson {
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "gen", "status": "success" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "validate", "status": "success" })
                            );
                            println!(
                                "{}",
                                serde_json::json!({ "event": "step_finished", "step": "ans", "status": "failed" })
                            );
                        }
                    }
                }
                if format == OutputFormat::Human {
                    eprintln!(
                        "Pipeline stopped: step 'uj {s_name}' failed with status {:?}",
                        runner.final_code
                    );
                }
            }
            crate::author_steps::CaseStagesOutcome::Interrupted => {
                runner.final_code = ExitCode::from(EXIT_INTERRUPTED);
            }
        }
    }

    if format == OutputFormat::Json {
        let is_ok = runner.failed_step.is_none();
        let res = serde_json::json!({
            "ok": is_ok,
            "steps": runner.step_records,
            "failed_step": runner.failed_step
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Ndjson {
        println!(
            "{}",
            serde_json::json!({
                "event": "pipeline_finished",
                "ok": runner.failed_step.is_none()
            })
        );
    } else if format == OutputFormat::Human && runner.failed_step.is_none() {
        println!("Pipeline completed successfully!");
    }

    runner.final_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use uj_core::config::{ProblemConfig, SolutionConfig, SolutionTag};
    use uj_core::verdict::Verdict;
    use uj_report::result::{SingleTestResult, SubmissionJudgeReport, SubtaskResult, TimingInfo};

    fn make_cfg() -> ProblemConfig {
        ProblemConfig::from_toml_str(
            r#"
            schema = 1
            name = "test_prob"
            type = "traditional"
            [limits]
            time = "1s"
            memory = "256MiB"
            "#,
            None,
        )
        .unwrap()
    }

    fn make_report(
        verdict: Verdict,
        score: f64,
        tests: Vec<Verdict>,
        subtasks: Vec<(u32, Verdict)>,
    ) -> SubmissionJudgeReport {
        SubmissionJudgeReport {
            schema_version: 1,
            problem: "test_prob".to_string(),
            submission: "test_sol".to_string(),
            verdict,
            score,
            max_time_ms: 10,
            max_memory_kb: 1024,
            subtasks: subtasks
                .into_iter()
                .map(|(id, v)| SubtaskResult {
                    id,
                    score: if v == Verdict::Accepted { 100.0 } else { 0.0 },
                    max_score: 100.0,
                    verdict: v,
                    test_ids: vec![],
                })
                .collect(),
            tests: tests
                .into_iter()
                .enumerate()
                .map(|(i, v)| SingleTestResult {
                    id: format!("{}", i + 1),
                    subtask: None,
                    verdict: v,
                    time_ms: 10,
                    wall_ms: 10,
                    memory_kb: 1024,
                    score: if v == Verdict::Accepted { 10.0 } else { 0.0 },
                    exit: uj_report::result::ExitInfo {
                        code: Some(0),
                        signal: None,
                        nt_status: None,
                    },
                    message: None,
                    checker: None,
                    diff: None,
                    timing: TimingInfo::default(),
                    sandbox_tier: None,
                })
                .collect(),
        }
    }

    #[test]
    fn test_tag_main_and_accepted() {
        let cfg = make_cfg();
        let sol_main = SolutionConfig {
            path: PathBuf::from("main.cpp"),
            tag: SolutionTag::Main,
            expect_subtasks: None,
        };
        let sol_ac = SolutionConfig {
            path: PathBuf::from("ac.cpp"),
            tag: SolutionTag::Accepted,
            expect_subtasks: None,
        };

        let rep_ac = make_report(Verdict::Accepted, 100.0, vec![Verdict::Accepted], vec![]);
        let rep_wa = make_report(
            Verdict::WrongAnswer,
            0.0,
            vec![Verdict::WrongAnswer],
            vec![],
        );

        assert!(check_solution_tag(&sol_main, &rep_ac, &cfg).is_empty());
        assert!(!check_solution_tag(&sol_main, &rep_wa, &cfg).is_empty());

        assert!(check_solution_tag(&sol_ac, &rep_ac, &cfg).is_empty());
        assert!(!check_solution_tag(&sol_ac, &rep_wa, &cfg).is_empty());
    }

    #[test]
    fn test_tag_wrong_answer() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("wa.cpp"),
            tag: SolutionTag::WrongAnswer,
            expect_subtasks: None,
        };

        // Passes when there's WA and no fatal
        let rep_wa = make_report(
            Verdict::WrongAnswer,
            50.0,
            vec![Verdict::Accepted, Verdict::WrongAnswer],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_wa, &cfg).is_empty());

        // Fails when pure AC
        let rep_ac = make_report(Verdict::Accepted, 100.0, vec![Verdict::Accepted], vec![]);
        assert!(!check_solution_tag(&sol, &rep_ac, &cfg).is_empty());

        // Fails when fatal error (TLE) present
        let rep_tle = make_report(
            Verdict::TimeLimitExceeded,
            0.0,
            vec![Verdict::WrongAnswer, Verdict::TimeLimitExceeded],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_tle, &cfg).is_empty());
    }

    #[test]
    fn test_tag_time_limit() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("tle.cpp"),
            tag: SolutionTag::TimeLimit,
            expect_subtasks: None,
        };

        let rep_tle = make_report(
            Verdict::TimeLimitExceeded,
            50.0,
            vec![Verdict::Accepted, Verdict::TimeLimitExceeded],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_tle, &cfg).is_empty());

        // Fails if has WA
        let rep_wa = make_report(
            Verdict::WrongAnswer,
            0.0,
            vec![Verdict::WrongAnswer, Verdict::TimeLimitExceeded],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_wa, &cfg).is_empty());

        // Fails if AC
        let rep_ac = make_report(Verdict::Accepted, 100.0, vec![Verdict::Accepted], vec![]);
        assert!(!check_solution_tag(&sol, &rep_ac, &cfg).is_empty());
    }

    #[test]
    fn test_tag_memory_limit() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("mle.cpp"),
            tag: SolutionTag::MemoryLimit,
            expect_subtasks: None,
        };

        let rep_mle = make_report(
            Verdict::MemoryLimitExceeded,
            50.0,
            vec![Verdict::Accepted, Verdict::MemoryLimitExceeded],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_mle, &cfg).is_empty());

        let rep_re = make_report(
            Verdict::RuntimeError,
            0.0,
            vec![Verdict::MemoryLimitExceeded, Verdict::RuntimeError],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_re, &cfg).is_empty());
    }

    #[test]
    fn test_tag_runtime_error() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("re.cpp"),
            tag: SolutionTag::RuntimeError,
            expect_subtasks: None,
        };

        let rep_re = make_report(
            Verdict::RuntimeError,
            50.0,
            vec![Verdict::Accepted, Verdict::RuntimeError],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_re, &cfg).is_empty());

        let rep_wa = make_report(
            Verdict::WrongAnswer,
            0.0,
            vec![Verdict::WrongAnswer, Verdict::RuntimeError],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_wa, &cfg).is_empty());
    }

    #[test]
    fn test_tag_rejected() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("bad.cpp"),
            tag: SolutionTag::Rejected,
            expect_subtasks: None,
        };

        let rep_wa = make_report(
            Verdict::WrongAnswer,
            0.0,
            vec![Verdict::WrongAnswer],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_wa, &cfg).is_empty());

        let rep_ac = make_report(Verdict::Accepted, 100.0, vec![Verdict::Accepted], vec![]);
        assert!(!check_solution_tag(&sol, &rep_ac, &cfg).is_empty());

        let rep_jf = make_report(
            Verdict::JudgeFailed,
            0.0,
            vec![Verdict::JudgeFailed],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_jf, &cfg).is_empty());
    }

    #[test]
    fn test_tag_partial() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("partial.cpp"),
            tag: SolutionTag::Partial,
            expect_subtasks: None,
        };

        let rep_part = make_report(
            Verdict::PartiallyCorrect,
            50.0,
            vec![Verdict::Accepted, Verdict::WrongAnswer],
            vec![],
        );
        assert!(check_solution_tag(&sol, &rep_part, &cfg).is_empty());

        let rep_zero = make_report(
            Verdict::WrongAnswer,
            0.0,
            vec![Verdict::WrongAnswer],
            vec![],
        );
        assert!(!check_solution_tag(&sol, &rep_zero, &cfg).is_empty());

        let rep_full = make_report(Verdict::Accepted, 100.0, vec![Verdict::Accepted], vec![]);
        assert!(!check_solution_tag(&sol, &rep_full, &cfg).is_empty());
    }

    #[test]
    fn test_tag_subtask_expectations() {
        let cfg = make_cfg();
        let mut expect = std::collections::HashMap::new();
        expect.insert(1, Verdict::Accepted);
        expect.insert(2, Verdict::TimeLimitExceeded);

        let sol = SolutionConfig {
            path: PathBuf::from("subtasks.cpp"),
            tag: SolutionTag::TimeLimit,
            expect_subtasks: Some(expect),
        };

        let rep_ok = make_report(
            Verdict::TimeLimitExceeded,
            50.0,
            vec![Verdict::Accepted, Verdict::TimeLimitExceeded],
            vec![(1, Verdict::Accepted), (2, Verdict::TimeLimitExceeded)],
        );
        assert!(check_solution_tag(&sol, &rep_ok, &cfg).is_empty());

        let rep_mismatch = make_report(
            Verdict::TimeLimitExceeded,
            50.0,
            vec![Verdict::Accepted, Verdict::TimeLimitExceeded],
            vec![(1, Verdict::Accepted), (2, Verdict::WrongAnswer)],
        );
        let failures = check_solution_tag(&sol, &rep_mismatch, &cfg);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("subtask 2 expected 'TimeLimitExceeded'"));
    }
}

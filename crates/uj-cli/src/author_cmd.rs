//! Authoring workflow commands: init, build, gen, validate, ans, verify, tl, stress, pipeline.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{ProblemConfig, SolutionTag};
use uj_core::run::{KillReason, Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_engine::compiler::CompileSession;
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_sandbox::{CancelToken, create_sandbox};

use crate::cli::{Cli, OutputFormat, SandboxMode};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS,
    EXIT_USAGE_OR_CONFIG, EXIT_VERIFY_MISMATCH, handle_config_error,
};

const VENDORED_TESTLIB_H: &str = include_str!("../../../assets/testlib.h");

fn safe_write_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
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
    let _ = fs::create_dir_all(&tests_dir);

    // Find starting test index: only scan existing tests if append is requested
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

    struct GenJob {
        idx: u32,
        gen_name: String,
        gen_args: String,
        argv: Vec<std::ffi::OsString>,
        target_in: PathBuf,
        cache_meta: PathBuf,
        hash: String,
    }

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

    let mut jobs = Vec::new();
    for gen_spec in &cfg.tests.generators {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }
        let gen_path = problem_dir.join(&gen_spec.generator);
        let compiled_gen = match session.compile(&gen_path, false) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to compile generator '{}': {e}", gen_path.display());
                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        };

        let mut base_hasher = blake3::Hasher::new();
        if let Ok(b) = fs::read(&compiled_gen.binary_path) {
            base_hasher.update(&b);
        }
        base_hasher.update(gen_spec.args.as_bytes());

        for i in 0..gen_spec.count {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }
            max_idx += 1;
            let target_in = tests_dir.join(format!("{max_idx}.in"));

            // Blake3 cache check using cloned base hasher
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

                let limits = Limits::new(5000, 512 * 1024 * 1024);
                let spec = RunSpec {
                    argv: job.argv,
                    env: vec![],
                    work_dir: work_dir.clone(),
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

                let res = match sandbox.run(&spec, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = res_tx.send(GenResult::Error {
                            err: format!("Generator execution error: {e}"),
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                };

                if !res.status.is_success() {
                    let err_tail = String::from_utf8_lossy(&res.stderr_tail).to_string();
                    let _ = res_tx.send(GenResult::Failed {
                        idx: job.idx,
                        gen_name: job.gen_name,
                        gen_args: job.gen_args,
                        msg: err_tail,
                    });
                    stop.store(true, Ordering::Relaxed);
                    break;
                }

                let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
                if let Err(e) = safe_write_file(&job.target_in, &stdout_bytes) {
                    let _ = res_tx.send(GenResult::Error {
                        err: format!("Failed to write {}: {e}", job.target_in.display()),
                    });
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                let _ = safe_write_file(&job.cache_meta, job.hash.as_bytes());
                let _ = res_tx.send(GenResult::Generated);
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

                let mut argv = vec![val_bin.as_os_str().to_os_string()];
                if let Some(st_id) = job.subtask_id {
                    argv.push(std::ffi::OsString::from("--group"));
                    argv.push(std::ffi::OsString::from(format!("{st_id}")));
                }

                let limits = Limits::new(5000, 512 * 1024 * 1024);
                let spec = RunSpec {
                    argv,
                    env: vec![],
                    work_dir: p_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(job.input_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits,
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: true,
                };

                let res = match sandbox.run(&spec, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = res_tx.send(ValResult::Error {
                            err: format!("Validator runtime error: {e}"),
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                };

                if !res.status.is_success() {
                    let msg = String::from_utf8_lossy(&res.stderr_tail).trim().to_string();
                    let _ = res_tx.send(ValResult::Failed {
                        idx: job.idx,
                        name: job.name,
                        msg,
                    });
                    stop.store(true, Ordering::Relaxed);
                    break;
                } else {
                    let _ = res_tx.send(ValResult::Ok);
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

    let hash_file_streaming = |path: &Path| -> std::io::Result<String> {
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
    };

    let main_hash = match hash_file_streaming(&compiled.binary_path) {
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
        cache_meta: PathBuf,
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

        let in_hash = match hash_file_streaming(&test.input_path) {
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
            cache_meta,
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

                let spec = RunSpec {
                    argv: vec![sol_bin.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: p_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(job.input_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: limits.clone(),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };

                let mut res = match sandbox.run(&spec, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = res_tx.send(AnsResult::Error {
                            err: format!("Execution error: {e}"),
                        });
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                };

                // Cold-start Defender / scheduling stall retry:
                if res.killed_by == Some(KillReason::WallLimit)
                    && !cancel.is_cancelled()
                    && (res.cpu_time_us / 1000) < limits.cpu_time_ms
                {
                    let mut retry_spec = spec.clone();
                    retry_spec.limits.wall_time_ms = retry_spec.limits.wall_time_ms.max(15000);
                    if let Ok(retry_res) = sandbox.run(&retry_spec, &cancel) {
                        res = retry_res;
                    }
                }

                if !res.status.is_success() || res.killed_by.is_some() {
                    let reason = res
                        .killed_by
                        .map(|k| format!("{k:?}"))
                        .unwrap_or_else(|| format!("{:?}", res.status));
                    let _ = res_tx.send(AnsResult::Failed {
                        idx: job.idx,
                        name: job.name,
                        reason,
                    });
                    stop.store(true, Ordering::Relaxed);
                    break;
                }

                let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
                if let Err(e) = safe_write_file(&job.ans_path, &stdout_bytes) {
                    let _ = res_tx.send(AnsResult::Error {
                        err: format!("Failed to write ans file {}: {e}", job.ans_path.display()),
                    });
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                let _ = safe_write_file(&job.cache_meta, &stdout_bytes);
                let _ = res_tx.send(AnsResult::Generated);
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

    let mut verification_failures = Vec::new();

    let total_sols = cfg.solutions.len();
    for (idx, sol) in cfg.solutions.iter().enumerate() {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let is_positive = matches!(sol.tag, SolutionTag::Main | SolutionTag::Accepted);
        let options = JudgeOptions {
            sandbox_mode: sandbox_mode_str.to_string(),
            require_tier: cli.require_tier,
            fail_fast: cli.fail_fast,
            precise: cli.precise && is_positive,
        };
        let scheduler = Scheduler::with_session(options, session.clone());

        let sol_num = idx + 1;
        let tag_name = format!("{:?}", sol.tag);

        let sol_path = problem_dir.join(&sol.path);
        let compiled = match session.compile(&sol_path, cli.no_cache) {
            Ok(c) => c,
            Err(e) => {
                if format == OutputFormat::Human {
                    eprintln!(
                        "[{sol_num}/{total_sols}] {} ({tag_name}) -> COMPILATION FAILED: {e}",
                        sol.path.display()
                    );
                }
                verification_failures
                    .push(format!("{}: compilation failed: {e}", sol.path.display()));
                continue;
            }
        };

        let sol_name = sol.path.file_name().unwrap_or_default().to_string_lossy();
        let (event_sender, event_thread) = if cli.tui {
            let (tx, rx) = crossbeam_channel::unbounded::<uj_report::result::JudgeEvent>();
            let prob_name = cfg.name.clone();
            let sub_name = format!("{} ({tag_name})", sol.path.display());
            let cancel_clone = cancel.clone();
            let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
            let limits = cfg
                .to_limits()
                .unwrap_or_else(|_| Limits::new(1000, 256 * 1024 * 1024));
            let slot_count = uj_engine::topology::Topology::detect(limits.memory_bytes, cli.jobs)
                .slots
                .len();

            let handle = std::thread::spawn(move || {
                let _ = uj_tui::run_tui(
                    &prob_name,
                    &sub_name,
                    &test_names,
                    slot_count,
                    rx,
                    cancel_clone,
                    true,
                );
            });
            (Some(tx), Some(handle))
        } else if format == OutputFormat::Human && !cli.quiet {
            let (tx, rx) = crossbeam_channel::unbounded::<uj_report::result::JudgeEvent>();
            let total_cnt = tests.len() as u64;
            let sol_disp = sol.path.display().to_string();
            let handle = std::thread::spawn(move || {
                let pb = ProgressBar::new(total_cnt);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(&format!(
                            "{{spinner:.green}} [{sol_num}/{total_sols}] {sol_disp} ({{msg}}) [{{bar:25.cyan/blue}}] {{pos}}/{{len}}"
                        ))
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("=>-"),
                );
                pb.set_message("testing...");
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

        let report = match scheduler.judge_submission(
            problem_dir,
            &cfg,
            &compiled,
            &sol_name,
            tests.clone(),
            cli.jobs,
            event_sender,
            cancel,
        ) {
            Ok(r) => r,
            Err(e) => {
                if let Some(h) = event_thread {
                    let _ = h.join();
                }
                if format == OutputFormat::Human {
                    eprintln!(
                        "[{sol_num}/{total_sols}] {} ({tag_name}) -> JUDGE FAILED: {e}",
                        sol.path.display()
                    );
                }
                verification_failures.push(format!("{}: judging failed: {e}", sol.path.display()));
                continue;
            }
        };

        if let Some(h) = event_thread {
            let _ = h.join();
        }

        // Check against expected SolutionTag
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
            verification_failures.push(format!(
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
                    verification_failures.push(format!(
                        "{} subtask {st_id} expected '{expected_v:?}' but got '{actual_v:?}'",
                        sol.path.display()
                    ));
                }
            }
        }

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

    let mut step_records = Vec::new();
    let mut failed_step: Option<String> = None;
    let mut final_code = ExitCode::from(EXIT_SUCCESS);

    let mut step = |name: &'static str, run_fn: &dyn Fn() -> ExitCode| -> bool {
        if cancel.is_cancelled() {
            final_code = ExitCode::from(EXIT_INTERRUPTED);
            return false;
        }
        if format == OutputFormat::Human {
            println!("==> Step: uj {name}");
        } else if format == OutputFormat::Ndjson {
            println!(
                "{}",
                serde_json::json!({ "event": "step_started", "step": name })
            );
        }

        let code = run_fn();
        let success = code == ExitCode::from(EXIT_SUCCESS);
        step_records.push(serde_json::json!({
            "name": name,
            "status": if success { "success" } else { "failed" }
        }));

        if format == OutputFormat::Ndjson {
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
            failed_step = Some(name.to_string());
            final_code = code;
            if format == OutputFormat::Human {
                eprintln!("Pipeline stopped: step 'uj {name}' failed with status {code:?}");
            }
            return false;
        }
        true
    };

    if step("build", &|| {
        run_build_with_session(problem_dir, &[], cli.no_cache, sub_format, &session)
    }) && step("gen", &|| {
        run_gen_with_session(problem_dir, false, false, sub_format, cancel, &session)
    }) && step("validate", &|| {
        run_validate_with_session(problem_dir, sub_format, cancel, &session)
    }) && step("ans", &|| {
        run_ans_with_session(problem_dir, false, sub_format, cancel, &session)
    }) && step("verify", &|| {
        run_verify_with_session(problem_dir, cli, sub_format, cancel, &session)
    }) {
        step("tl", &|| {
            run_tl_with_session(problem_dir, false, cli, sub_format, cancel, &session)
        });
    }

    if format == OutputFormat::Json {
        let is_ok = failed_step.is_none();
        let res = serde_json::json!({
            "ok": is_ok,
            "steps": step_records,
            "failed_step": failed_step
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Ndjson {
        println!(
            "{}",
            serde_json::json!({
                "event": "pipeline_finished",
                "ok": failed_step.is_none()
            })
        );
    } else if format == OutputFormat::Human && failed_step.is_none() {
        println!("Pipeline completed successfully!");
    }

    final_code
}

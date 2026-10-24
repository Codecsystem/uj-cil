//! Authoring workflow commands: init, build, gen, validate, ans, verify, tl, stress, pipeline.

use std::fs;
use std::io::IsTerminal;
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

use crate::cli::{Cli, OutputFormat};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS,
    EXIT_USAGE_OR_CONFIG, EXIT_VERIFY_MISMATCH, atomic_write_bytes, emit_compile_error, emit_error,
    emit_interrupted, handle_config_error_fmt, load_problem,
};

const VENDORED_TESTLIB_H: &str = include_str!("../../../assets/testlib.h");

pub(crate) fn safe_write_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    atomic_write_bytes(path, content)
}

fn print_structured(format: OutputFormat, val: &serde_json::Value) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(val).unwrap());
        }
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(val).unwrap());
        }
        OutputFormat::Human | OutputFormat::Silent => {}
    }
}

// ============================================================================
// 1. uj init
// ============================================================================

pub fn run_init(
    dir: &Path,
    problem_type: &str,
    name_opt: Option<&str>,
    title_opt: Option<&str>,
    force: bool,
    format: OutputFormat,
) -> ExitCode {
    let ptype_norm = match problem_type.trim().to_ascii_lowercase().as_str() {
        "traditional" => "traditional",
        "interactive" => "interactive",
        "grader" => "grader",
        "communication" => "communication",
        "output_only" | "output-only" => "output_only",
        other => {
            emit_error(
                format,
                "InvalidProblemType",
                format!(
                    "Invalid problem type '{other}'. Expected one of: traditional, interactive, grader, communication, output_only."
                ),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let problem_toml_path = dir.join("problem.toml");
    if problem_toml_path.exists() && !force {
        emit_error(
            format,
            "AlreadyExists",
            format!(
                "Problem configuration already exists at '{}'. Refusing to overwrite without --force.",
                problem_toml_path.display()
            ),
            Some("Pass --force to overwrite existing problem files."),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let raw_name = name_opt
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s != "." && s != "..")
        })
        .unwrap_or_else(|| "sample_problem".to_string());

    let display_title = title_opt
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(&raw_name);

    let name_toml = toml::Value::String(raw_name.clone()).to_string();
    let type_toml = toml::Value::String(ptype_norm.to_string()).to_string();
    let type_extra_header = match ptype_norm {
        "interactive" => "interactor = \"interactor.cpp\"\n",
        "communication" => {
            "manager = \"manager.cpp\"\nprocesses = 2\ncommunication_topology = \"star\"\n"
        }
        _ => "",
    };
    let grader_section = if ptype_norm == "grader" {
        "\n[grader]\nfiles = [\"grader.cpp\"]\nheaders = [\"grader.h\"]\n"
    } else {
        ""
    };
    let title_comment = if let Some(t) = title_opt.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        format!("# Title: {}\n", t.replace(['\r', '\n'], " "))
    } else {
        String::new()
    };

    let problem_toml = format!(
        r#"{title_comment}schema = 1
name = {name_toml}
type = {type_toml}
validator = "validator.cpp"
{type_extra_header}
[limits]
time = "1.0s"
memory = "256MB"
{grader_section}
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

    if let Err(cfg_err) = ProblemConfig::from_toml_str(&problem_toml, Some(&problem_toml_path)) {
        return handle_config_error_fmt(cfg_err, format);
    }

    if let Err(e) = fs::create_dir_all(dir.join("solutions")) {
        emit_error(
            format,
            "IoError",
            format!("Failed to create solutions dir: {e}"),
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }
    if let Err(e) = fs::create_dir_all(dir.join("tests")) {
        emit_error(
            format,
            "IoError",
            format!("Failed to create tests dir: {e}"),
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    // Write statement.md
    let statement = format!(
        r#"# {display_title}

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

    let mut files_to_write: Vec<(PathBuf, &[u8])> = vec![
        (problem_toml_path, problem_toml.as_bytes()),
        (dir.join("statement.md"), statement.as_bytes()),
        (dir.join("testlib.h"), VENDORED_TESTLIB_H.as_bytes()),
        (dir.join("gen.cpp"), gen_cpp.as_bytes()),
        (dir.join("validator.cpp"), val_cpp.as_bytes()),
        (dir.join("solutions").join("std.cpp"), std_cpp.as_bytes()),
        (
            dir.join("solutions").join("brute.cpp"),
            brute_cpp.as_bytes(),
        ),
        (
            dir.join("solutions").join("wrong.cpp"),
            wrong_cpp.as_bytes(),
        ),
        (dir.join("tests").join("1.in"), b"1 2\n"),
        (dir.join("tests").join("1.ans"), b"3\n"),
    ];

    if ptype_norm == "interactive" {
        files_to_write.push((
            dir.join("interactor.cpp"),
            b"#include \"testlib.h\"\nint main(int argc, char* argv[]) {\n    registerInteraction(argc, argv);\n    quitf(_ok, \"ok\");\n}\n",
        ));
    } else if ptype_norm == "grader" {
        files_to_write.push((
            dir.join("grader.h"),
            b"#pragma once\nlong long solve(long long a, long long b);\n",
        ));
        files_to_write.push((
            dir.join("grader.cpp"),
            b"#include \"grader.h\"\n#include <iostream>\nint main() { long long a, b; if (std::cin >> a >> b) std::cout << solve(a, b) << std::endl; return 0; }\n",
        ));
    } else if ptype_norm == "communication" {
        files_to_write.push((
            dir.join("manager.cpp"),
            b"#include <iostream>\nint main() { std::cout << 1.0 << std::endl; return 0; }\n",
        ));
    }

    for (target_path, bytes) in files_to_write {
        if let Err(e) = safe_write_file(&target_path, bytes) {
            emit_error(
                format,
                "IoError",
                format!("Failed to write '{}': {e}", target_path.display()),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    }

    let res = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "problem": raw_name,
        "title": display_title,
        "type": ptype_norm,
        "dir": dir.to_string_lossy()
    });
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => {
            println!("Initialized problem '{}' in '{}'", raw_name, dir.display());
        }
        OutputFormat::Silent => {}
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
    let (code, val) = build_core(problem_dir, targets, no_cache, format, compile_session);
    if matches!(format, OutputFormat::Json | OutputFormat::Ndjson) {
        print_structured(format, &val);
    }
    code
}

pub fn build_core(
    problem_dir: &Path,
    targets: &[String],
    no_cache: bool,
    format: OutputFormat,
    compile_session: &CompileSession,
) -> (ExitCode, serde_json::Value) {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Cannot read problem.toml: {e}");
            if format == OutputFormat::Human {
                eprintln!("{msg}");
            }
            return (
                ExitCode::from(EXIT_USAGE_OR_CONFIG),
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": {
                        "kind": "ConfigError",
                        "message": msg,
                    }
                }),
            );
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            if format == OutputFormat::Human {
                eprintln!("Error: {e}");
            }
            let env = crate::error::StructuredErrorEnvelope::new(e);
            return (
                ExitCode::from(EXIT_USAGE_OR_CONFIG),
                serde_json::to_value(env).unwrap(),
            );
        }
    };

    let mut to_compile: Vec<(PathBuf, bool)> = Vec::new();
    let sol_paths: std::collections::HashSet<PathBuf> = cfg
        .solutions
        .iter()
        .map(|sol| {
            if sol.path.is_absolute() {
                sol.path.clone()
            } else {
                problem_dir.join(&sol.path)
            }
        })
        .collect();

    if targets.is_empty() {
        if let Some(ref v) = cfg.validator {
            to_compile.push((
                if v.is_absolute() {
                    v.clone()
                } else {
                    problem_dir.join(v)
                },
                false,
            ));
        }
        if let Some(ref chk) = cfg.checker.path {
            to_compile.push((
                if chk.is_absolute() {
                    chk.clone()
                } else {
                    problem_dir.join(chk)
                },
                false,
            ));
        }
        if let Some(ref int) = cfg.interactor {
            to_compile.push((
                if int.is_absolute() {
                    int.clone()
                } else {
                    problem_dir.join(int)
                },
                false,
            ));
        }
        if let Some(ref mgr) = cfg.manager {
            to_compile.push((
                if mgr.is_absolute() {
                    mgr.clone()
                } else {
                    problem_dir.join(mgr)
                },
                false,
            ));
        }
        for g in &cfg.tests.generators {
            let p = problem_dir.join(&g.generator);
            if p.exists() && !to_compile.iter().any(|(tp, _)| tp == &p) {
                to_compile.push((p, false));
            }
        }
        for sol in &cfg.solutions {
            let p = if sol.path.is_absolute() {
                sol.path.clone()
            } else {
                problem_dir.join(&sol.path)
            };
            if p.exists() && !to_compile.iter().any(|(tp, _)| tp == &p) {
                to_compile.push((p, true));
            }
        }
    } else {
        for t in targets {
            let p = problem_dir.join(t);
            let is_sol = sol_paths.contains(&p) || t.replace('\\', "/").starts_with("solutions/");
            to_compile.push((p, is_sol));
        }
    }

    to_compile.retain(|(target, _)| {
        !target
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    });

    if to_compile.is_empty() {
        return (
            ExitCode::from(EXIT_SUCCESS),
            serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "status": "Build succeeded",
                "compiled": []
            }),
        );
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

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<(PathBuf, bool)>();
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<BuildMsg>();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    for item in to_compile {
        let _ = job_tx.send(item);
    }
    drop(job_tx);

    let cfg_arc = Arc::new(cfg);
    let p_dir_arc = problem_dir.to_path_buf();
    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let session = compile_session.clone();
        let cfg_w = Arc::clone(&cfg_arc);
        let p_dir_w = p_dir_arc.clone();

        let handle = std::thread::spawn(move || {
            while let Ok((target, is_sol)) = job_rx.recv() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let comp_res = if is_sol {
                    session.compile_problem_solution(&p_dir_w, &cfg_w, &target, no_cache)
                } else {
                    session.compile(&target, no_cache)
                };
                match comp_res {
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

    let mut compiled_list = Vec::new();
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
                compiled_list.push(target.to_string_lossy().to_string());
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
        if format == OutputFormat::Human {
            eprintln!("Failed to compile {}: {e}", target.display());
        }
        return (
            ExitCode::from(EXIT_COMPILE_ERROR),
            serde_json::json!({
                "schema_version": 1,
                "ok": false,
                "error": {
                    "kind": "CompileError",
                    "target": target.to_string_lossy(),
                    "message": e
                }
            }),
        );
    }

    (
        ExitCode::from(EXIT_SUCCESS),
        serde_json::json!({
            "schema_version": 1,
            "ok": true,
            "status": "Build succeeded",
            "compiled": compiled_list
        }),
    )
}

// ============================================================================
// 3. uj gen
// ============================================================================

pub fn run_gen(
    problem_dir: &Path,
    force: bool,
    append: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_gen_with_session(problem_dir, force, append, cli, format, cancel, &session)
}

pub fn run_gen_with_session(
    problem_dir: &Path,
    force: bool,
    append: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    uj_engine::clear_answer_cache();
    let (_config_path, _content, cfg) = match load_problem(problem_dir, format) {
        Ok(res) => res,
        Err(code) => return code,
    };

    if cfg.tests.generators.is_empty() {
        let res = serde_json::json!({ "schema_version": 1, "ok": true, "generated": 0 });
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
            OutputFormat::Human => println!("No generator configured in problem.toml"),
            OutputFormat::Silent => {}
        }
        return ExitCode::from(EXIT_SUCCESS);
    }

    let tests_dir = problem_dir.join(&cfg.tests.dir);
    let jobs =
        match crate::author_steps::plan_gen_jobs(problem_dir, &cfg, force, append, session, cancel)
        {
            Ok(j) => j,
            Err(code) => {
                if cancel.is_cancelled() {
                    return emit_interrupted(format, "Test generation");
                }
                emit_error(
                    format,
                    "CompileError",
                    "Failed to compile generator(s)",
                    None,
                );
                return code;
            }
        };

    if jobs.is_empty() {
        let res = serde_json::json!({ "schema_version": 1, "ok": true, "generated": 0 });
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
            OutputFormat::Human => println!("All tests already generated or cached"),
            OutputFormat::Silent => {}
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
    let sandbox_mode_str = cli.sandbox_mode_str().to_string();
    let require_tier = cli.require_tier;
    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let work_dir = t_dir.clone();
        let sb_mode = sandbox_mode_str.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox(&sb_mode, require_tier) {
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
        return emit_interrupted(format, "Test generation");
    }

    if let Some(err) = runtime_error {
        emit_error(format, "SandboxError", err, None);
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, gen_name, gen_args, msg)) = first_failure {
        emit_error(
            format,
            "GeneratorError",
            format!("Generator '{gen_name}' failed with args '{gen_args}': {msg}"),
            None,
        );
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    let res = serde_json::json!({ "schema_version": 1, "ok": true, "generated": generated_count });
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => {
            println!(
                "Generated {generated_count} test case(s) in '{}'",
                tests_dir.display()
            );
        }
        OutputFormat::Silent => {}
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 4. uj validate
// ============================================================================

pub fn run_validate(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_validate_with_session(problem_dir, cli, format, cancel, &session)
}

pub fn run_validate_with_session(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    uj_engine::clear_answer_cache();
    let (_config_path, _content, cfg) = match load_problem(problem_dir, format) {
        Ok(res) => res,
        Err(code) => return code,
    };

    let Some(ref val_path_rel) = cfg.validator else {
        let res = serde_json::json!({ "schema_version": 1, "ok": true, "validated": 0 });
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
            OutputFormat::Human => println!("No validator configured in problem.toml"),
            OutputFormat::Silent => {}
        }
        return ExitCode::from(EXIT_SUCCESS);
    };

    let val_path = problem_dir.join(val_path_rel);
    let compiled_val = match session.compile(&val_path, false) {
        Ok(c) => c,
        Err(e) => {
            return emit_compile_error(format, Some(&val_path), e.to_string());
        }
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            emit_error(
                format,
                "DiscoverError",
                format!("Failed to discover tests: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
        .min(tests.len().max(1));

    if tests.is_empty() {
        let res = serde_json::json!({ "schema_version": 1, "ok": true, "validated": 0 });
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
            OutputFormat::Human => println!("No test cases to validate"),
            OutputFormat::Silent => {}
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
    let sandbox_mode_str = cli.sandbox_mode_str().to_string();
    let require_tier = cli.require_tier;

    let mut handles = Vec::new();
    for _ in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let val_bin = val_binary.clone();
        let p_dir = prob_dir.clone();
        let sb_mode = sandbox_mode_str.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox(&sb_mode, require_tier) {
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
        return emit_interrupted(format, "Validation");
    }

    if let Some(err) = runtime_error {
        emit_error(format, "SandboxError", err, None);
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, name, msg)) = first_failure {
        let err = serde_json::json!({
            "schema_version": 1,
            "ok": false,
            "error": {
                "kind": "ValidationError",
                "test": name,
                "message": msg
            }
        });
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &err),
            OutputFormat::Human => eprintln!("Validator failed on test '{name}': {msg}"),
            OutputFormat::Silent => {}
        }
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    let res = serde_json::json!({ "schema_version": 1, "ok": true, "validated": success_count });
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => {
            println!("All {} test case(s) successfully validated", success_count)
        }
        OutputFormat::Silent => {}
    }

    ExitCode::from(EXIT_SUCCESS)
}

// ============================================================================
// 5. uj ans
// ============================================================================

pub fn run_ans(
    problem_dir: &Path,
    force: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let session = CompileSession::new();
    run_ans_with_session(problem_dir, force, cli, format, cancel, &session)
}

pub fn run_ans_with_session(
    problem_dir: &Path,
    force: bool,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    uj_engine::clear_answer_cache();
    let (_config_path, _content, cfg) = match load_problem(problem_dir, format) {
        Ok(res) => res,
        Err(code) => return code,
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
            emit_error(
                format,
                "ConfigError",
                "No main or accepted solution configured to generate answers",
                None,
            );
            ExitCode::from(EXIT_USAGE_OR_CONFIG)
        });

    let main_sol = match main_sol {
        Ok(s) => s,
        Err(code) => return code,
    };

    let sol_path = problem_dir.join(&main_sol.path);
    let compiled = match session.compile_problem_solution(problem_dir, &cfg, &sol_path, false) {
        Ok(c) => c,
        Err(e) => {
            return emit_compile_error(format, Some(&sol_path), e.to_string());
        }
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            emit_error(
                format,
                "DiscoverError",
                format!("Failed to discover tests: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let main_hash = match crate::author_steps::hash_file_streaming(&compiled.binary_path) {
        Ok(h) => h,
        Err(e) => {
            emit_error(
                format,
                "IoError",
                format!("Failed to read binary for hash: {e}"),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };
    let limits = cfg
        .to_limits()
        .unwrap_or_else(|_| Limits::new(1000, 256 * 1024 * 1024));

    let topo = uj_engine::topology::Topology::detect(limits.memory_bytes, cli.jobs);
    let num_workers = topo.slots.len().max(1).min(tests.len().max(1));

    struct AnsJob {
        idx: usize,
        name: String,
        input_path: PathBuf,
        ans_path: PathBuf,
        in_hash: String,
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
            return emit_interrupted(format, "Answer generation");
        }

        let in_hash = match crate::author_steps::hash_file_streaming(&test.input_path) {
            Ok(h) => h,
            Err(e) => {
                emit_error(
                    format,
                    "IoError",
                    format!(
                        "Failed to read input file for hash {}: {e}",
                        test.input_path.display()
                    ),
                    None,
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
            in_hash,
        });
    }
    drop(job_tx);

    let sol_binary =
        uj_sandbox::get_or_prepare_authorized_binary(&compiled.binary_path, problem_dir)
            .unwrap_or_else(|_| compiled.binary_path.clone());
    let prob_dir = problem_dir.to_path_buf();
    let run_limits = limits.clone();
    let sandbox_mode_str = cli.sandbox_mode_str().to_string();
    let require_tier = cli.require_tier;

    let mut handles = Vec::new();
    for worker_idx in 0..num_workers {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let stop = stop_flag.clone();
        let cancel = cancel.clone();
        let sol_bin = sol_binary.clone();
        let p_dir = prob_dir.clone();
        let limits = run_limits.clone();
        let main_hash_w = main_hash.clone();
        let sb_mode = sandbox_mode_str.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox(&sb_mode, require_tier) {
                Ok(s) => s,
                Err(e) => {
                    let _ = res_tx.send(AnsResult::Error {
                        err: format!("Sandbox error: {e}"),
                    });
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let slot_box = match uj_sandbox::create_slot_box_dir(&p_dir, worker_idx) {
                Ok(b) => b,
                Err(e) => {
                    let _ = res_tx.send(AnsResult::Error {
                        err: format!("Failed to create sandbox box dir: {e}"),
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

                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                match crate::author_steps::exec_ans(
                    sandbox.as_ref(),
                    &sol_bin,
                    &main_hash_w,
                    &job.input_path,
                    &job.ans_path,
                    &p_dir,
                    &slot_box.box_dir,
                    Some(&job.in_hash),
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
        return emit_interrupted(format, "Answer generation");
    }

    if let Some(err) = runtime_error {
        emit_error(format, "SandboxError", err, None);
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    if let Some((_, name, reason)) = first_failure {
        emit_error(
            format,
            "AnswerGenerationError",
            format!("Main solution failed on test '{name}': {reason}"),
            None,
        );
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    let res = serde_json::json!({ "schema_version": 1, "ok": true, "answers_generated": generated_count });
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => println!("Generated {generated_count} answer file(s)"),
        OutputFormat::Silent => {}
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

    let is_extra_allowed = |v: Verdict| -> bool {
        !matches!(v, Verdict::JudgeFailed | Verdict::CompileError)
            && sol
                .allow_verdicts
                .as_ref()
                .is_some_and(|list| list.contains(&v))
    };

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
                        | Verdict::CompileError
                        | Verdict::JudgeFailed
                ) && !is_extra_allowed(t.verdict)
            });
            if report.verdict == Verdict::Accepted
                || report.verdict == Verdict::JudgeFailed
                || !has_wa
                || has_fatal
            {
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
                        | Verdict::CompileError
                        | Verdict::JudgeFailed
                ) && !is_extra_allowed(t.verdict)
            });
            if report.verdict == Verdict::Accepted
                || report.verdict == Verdict::JudgeFailed
                || !has_tle
                || has_other
            {
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
                        | Verdict::CompileError
                        | Verdict::JudgeFailed
                ) && !is_extra_allowed(t.verdict)
            });
            if report.verdict == Verdict::Accepted
                || report.verdict == Verdict::JudgeFailed
                || !has_mle
                || has_other
            {
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
                        | Verdict::CompileError
                        | Verdict::JudgeFailed
                ) && !is_extra_allowed(t.verdict)
            });
            if report.verdict == Verdict::Accepted
                || report.verdict == Verdict::JudgeFailed
                || !has_re
                || has_other
            {
                matches_tag = false;
            }
        }
        SolutionTag::WrongAnswerOrTimeLimit => {
            let has_wa = report
                .tests
                .iter()
                .any(|t| t.verdict == Verdict::WrongAnswer);
            let has_disallowed = report.tests.iter().any(|t| {
                !matches!(
                    t.verdict,
                    Verdict::Accepted
                        | Verdict::Skipped
                        | Verdict::WrongAnswer
                        | Verdict::TimeLimitExceeded
                ) && !is_extra_allowed(t.verdict)
            });
            if report.verdict == Verdict::Accepted
                || report.verdict == Verdict::JudgeFailed
                || !has_wa
                || has_disallowed
            {
                matches_tag = false;
            }
        }
        SolutionTag::Rejected => {
            if report.verdict == Verdict::Accepted || report.verdict == Verdict::JudgeFailed {
                matches_tag = false;
            } else if let Some(ref allowed) = sol.allow_verdicts
                && !allowed.is_empty()
            {
                let has_any_allowed = report.tests.iter().any(|t| is_extra_allowed(t.verdict));
                let has_disallowed = report.tests.iter().any(|t| {
                    !matches!(t.verdict, Verdict::Accepted | Verdict::Skipped)
                        && !is_extra_allowed(t.verdict)
                });
                if !has_any_allowed || has_disallowed {
                    matches_tag = false;
                }
            }
        }
        SolutionTag::Partial => {
            let max_score = cfg.max_score();
            if report.score <= 0.0 || report.score >= max_score {
                matches_tag = false;
            } else if let Some(ref allowed) = sol.allow_verdicts
                && !allowed.is_empty()
            {
                let has_disallowed = report.tests.iter().any(|t| {
                    !matches!(
                        t.verdict,
                        Verdict::Accepted | Verdict::PartiallyCorrect | Verdict::Skipped
                    ) && !is_extra_allowed(t.verdict)
                });
                if has_disallowed {
                    matches_tag = false;
                }
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

pub(crate) fn summarize_solution_report(
    sol: &SolutionConfig,
    report: &SubmissionJudgeReport,
    matches_tag: bool,
) -> serde_json::Value {
    let mut max_time_ms = 0u64;
    let mut max_time_test: Option<String> = None;
    let mut max_memory_kb = report.max_memory_kb;
    let mut first_fail_test: Option<String> = None;
    let mut first_fail_verdict: Option<Verdict> = None;

    for t in &report.tests {
        if max_time_test.is_none() || t.time_ms > max_time_ms {
            max_time_ms = t.time_ms;
            max_time_test = Some(t.id.clone());
        }
        if t.memory_kb > max_memory_kb {
            max_memory_kb = t.memory_kb;
        }
        if first_fail_test.is_none() && !t.verdict.is_accepted() && t.verdict != Verdict::Skipped {
            first_fail_test = Some(t.id.clone());
            first_fail_verdict = Some(t.verdict);
        }
    }
    if report.tests.is_empty() {
        max_time_ms = report.max_time_ms;
    }

    serde_json::json!({
        "path": sol.path.to_string_lossy().replace('\\', "/"),
        "tag": sol.tag.as_str(),
        "matches_tag": matches_tag,
        "verdict": report.verdict,
        "score": report.score,
        "max_time_ms": max_time_ms,
        "max_time_test": max_time_test,
        "max_memory_kb": max_memory_kb,
        "first_fail_test": first_fail_test,
        "first_fail_verdict": first_fail_verdict,
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedVerifyTlEntry {
    path: String,
    tag: SolutionTag,
    binary_hash: String,
    report: SubmissionJudgeReport,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedVerifyTlState {
    tests_fingerprint: String,
    entries: Vec<CachedVerifyTlEntry>,
}

fn compute_tests_fingerprint(tests: &[uj_core::tests::TestCase]) -> String {
    let mut hasher = blake3::Hasher::new();
    for t in tests {
        hasher.update(t.name.as_bytes());
        hasher.update(&[0]);
        if let Some(st) = t.subtask_id {
            hasher.update(&st.to_le_bytes());
        }
        if let Ok(meta) = fs::metadata(&t.input_path) {
            hasher.update(&meta.len().to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

pub fn run_verify_with_session(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> ExitCode {
    let (code, val) = verify_core(problem_dir, cli, format, cancel, session);
    if matches!(format, OutputFormat::Json | OutputFormat::Ndjson) {
        print_structured(format, &val);
    }
    code
}

pub fn verify_core(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    session: &CompileSession,
) -> (ExitCode, serde_json::Value) {
    let config_path = problem_dir.join("problem.toml");
    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Cannot read problem.toml: {e}");
            if format == OutputFormat::Human {
                eprintln!("{msg}");
            }
            return (
                ExitCode::from(EXIT_USAGE_OR_CONFIG),
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": {
                        "kind": "ConfigError",
                        "message": msg,
                    }
                }),
            );
        }
    };
    let cfg = match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            if format == OutputFormat::Human {
                eprintln!("Error: {e}");
            }
            let env = crate::error::StructuredErrorEnvelope::new(e);
            return (
                ExitCode::from(EXIT_USAGE_OR_CONFIG),
                serde_json::to_value(env).unwrap(),
            );
        }
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("Failed to discover tests: {e}");
            if format == OutputFormat::Human {
                eprintln!("{msg}");
            }
            return (
                ExitCode::from(EXIT_USAGE_OR_CONFIG),
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": {
                        "kind": "DiscoverError",
                        "message": msg,
                    }
                }),
            );
        }
    };

    let sandbox_mode_str = cli.sandbox_mode_str();

    let total_sols = cfg.solutions.len();
    if total_sols == 0 {
        if format == OutputFormat::Human {
            println!("All 0 solution(s) passed verification!");
        }
        return (
            ExitCode::from(EXIT_SUCCESS),
            serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "verified": 0,
                "solutions": []
            }),
        );
    }

    // Phase A: Parallel compilation & context preparation
    let problem_ctx = match ProblemContext::prepare(0, problem_dir, &cfg, session) {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            let msg = format!("Problem setup error: {e}");
            if format == OutputFormat::Human {
                eprintln!("{msg}");
            }
            return (
                ExitCode::from(EXIT_JUDGE_FAILED),
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": {
                        "kind": "ProblemSetupError",
                        "message": msg,
                    }
                }),
            );
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
                let compiled = match session_w.compile_problem_solution(
                    &problem_dir_w,
                    &p_ctx.config,
                    &sol_path,
                    no_cache,
                ) {
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
        if format == OutputFormat::Human {
            eprintln!("Verification interrupted");
        }
        return (
            ExitCode::from(EXIT_INTERRUPTED),
            serde_json::json!({
                "schema_version": 1,
                "ok": false,
                "error": {
                    "kind": "Interrupted",
                    "message": "Verification interrupted"
                }
            }),
        );
    }

    // Phase B / C: JudgePool evaluation
    let topology = uj_engine::topology::Topology::detect(0, cli.jobs);
    let slots = topology.slots.len();
    let window = std::cmp::max(2, slots / 2);
    let memory_budget = uj_engine::topology::Topology::memory_budget_bytes();

    let use_tui = cli.tui && format == OutputFormat::Human && std::io::stderr().is_terminal();

    let (ptx, prx) = crossbeam_channel::unbounded::<PoolEvent>();
    let pool_events_opt = if use_tui { Some(ptx.clone()) } else { None };

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

    let tui_handle = if use_tui {
        let slot_cpus: Vec<Option<usize>> = topology.slots.iter().map(|s| s.cpu_core).collect();
        let title = format!("Verify: {}", cfg.name);
        let cancel_tui = cancel.clone();
        let rx = prx.clone();
        Some(std::thread::spawn(move || {
            if let Err(e) = run_multi_tui(
                &title,
                &sub_metas,
                slots,
                slot_cpus,
                rx.clone(),
                cancel_tui,
                true,
            ) {
                eprintln!(
                    "Warning: Verify TUI initialization failed ({e}); falling back to non-TUI execution."
                );
                while rx.recv().is_ok() {}
            }
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

    let show_progress = format == OutputFormat::Human && !cli.quiet && !use_tui;
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
                precise: cli.effective_precise() && is_positive,
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

    if cancel.is_cancelled() {
        if format == OutputFormat::Human {
            eprintln!("Verification interrupted");
        }
        return (
            ExitCode::from(EXIT_INTERRUPTED),
            serde_json::json!({
                "schema_version": 1,
                "ok": false,
                "error": {
                    "kind": "Interrupted",
                    "message": "Verification interrupted"
                }
            }),
        );
    }

    let mut reports_by_sol_idx = std::collections::HashMap::new();
    for (sub_res, sol_idx) in pool_reports.into_iter().zip(sub_idx_to_sol_idx) {
        reports_by_sol_idx.insert(sol_idx, sub_res);
    }

    let mut verification_failures = Vec::new();
    let mut solutions_summary = Vec::with_capacity(cfg.solutions.len());
    let mut cached_tl_entries = Vec::new();
    let mut verified_ok_count = 0usize;

    // Verification result reporting in deterministic cfg.solutions order
    for (idx, sol) in cfg.solutions.iter().enumerate() {
        let sol_num = idx + 1;
        let tag_name = format!("{:?}", sol.tag);
        let path_norm = sol.path.to_string_lossy().replace('\\', "/");

        if let Some(err) = compile_errors.get(&idx) {
            if format == OutputFormat::Human {
                eprintln!(
                    "[{sol_num}/{total_sols}] {} ({tag_name}) -> COMPILATION FAILED: {err}",
                    sol.path.display()
                );
            }
            verification_failures
                .push(format!("{}: compilation failed: {err}", sol.path.display()));
            solutions_summary.push(serde_json::json!({
                "path": path_norm,
                "tag": sol.tag.as_str(),
                "matches_tag": false,
                "verdict": Verdict::CompileError,
                "score": 0.0,
                "max_time_ms": 0,
                "max_time_test": serde_json::Value::Null,
                "max_memory_kb": 0,
                "first_fail_test": serde_json::Value::Null,
                "first_fail_verdict": serde_json::Value::Null,
            }));
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
            solutions_summary.push(serde_json::json!({
                "path": path_norm,
                "tag": sol.tag.as_str(),
                "matches_tag": false,
                "verdict": Verdict::JudgeFailed,
                "score": 0.0,
                "max_time_ms": 0,
                "max_time_test": serde_json::Value::Null,
                "max_memory_kb": 0,
                "first_fail_test": serde_json::Value::Null,
                "first_fail_verdict": serde_json::Value::Null,
            }));
            continue;
        }

        match reports_by_sol_idx.remove(&idx) {
            Some(Ok(report)) => {
                let sol_failures = check_solution_tag(sol, &report, &cfg);
                let matches_tag = sol_failures.is_empty();
                if matches_tag {
                    verified_ok_count += 1;
                }
                verification_failures.extend(sol_failures);
                solutions_summary.push(summarize_solution_report(sol, &report, matches_tag));

                if matches!(sol.tag, SolutionTag::Main | SolutionTag::Accepted)
                    && report.verdict.is_accepted()
                    && let Some(sub_ctx) = ready_map.get(&idx)
                    && let Ok(bin_hash) =
                        crate::author_steps::hash_file_streaming(&sub_ctx.compiled_binary_path)
                {
                    cached_tl_entries.push(CachedVerifyTlEntry {
                        path: path_norm,
                        tag: sol.tag,
                        binary_hash: bin_hash,
                        report: report.clone(),
                    });
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
            Some(Err(e)) => {
                if format == OutputFormat::Human {
                    eprintln!(
                        "[{sol_num}/{total_sols}] {} ({tag_name}) -> JUDGE FAILED: {e}",
                        sol.path.display()
                    );
                }
                verification_failures.push(format!("{}: judging failed: {e}", sol.path.display()));
                solutions_summary.push(serde_json::json!({
                    "path": path_norm,
                    "tag": sol.tag.as_str(),
                    "matches_tag": false,
                    "verdict": Verdict::JudgeFailed,
                    "score": 0.0,
                    "max_time_ms": 0,
                    "max_time_test": serde_json::Value::Null,
                    "max_memory_kb": 0,
                    "first_fail_test": serde_json::Value::Null,
                    "first_fail_verdict": serde_json::Value::Null,
                }));
            }
            None => {
                verification_failures
                    .push(format!("{}: missing evaluation result", sol.path.display()));
                solutions_summary.push(serde_json::json!({
                    "path": path_norm,
                    "tag": sol.tag.as_str(),
                    "matches_tag": false,
                    "verdict": Verdict::JudgeFailed,
                    "score": 0.0,
                    "max_time_ms": 0,
                    "max_time_test": serde_json::Value::Null,
                    "max_memory_kb": 0,
                    "first_fail_test": serde_json::Value::Null,
                    "first_fail_verdict": serde_json::Value::Null,
                }));
            }
        }
    }

    if !cached_tl_entries.is_empty() && !cli.no_cache {
        let state = CachedVerifyTlState {
            tests_fingerprint: compute_tests_fingerprint(&tests),
            entries: cached_tl_entries,
        };
        let cache_file = problem_dir
            .join(".uj")
            .join("cache")
            .join("verify_tl_cache.json");
        if let Ok(json_bytes) = serde_json::to_vec(&state) {
            let _ = safe_write_file(&cache_file, &json_bytes);
        }
    }

    if !verification_failures.is_empty() {
        if format == OutputFormat::Human {
            eprintln!(
                "Verification failed for {} solution(s):",
                verification_failures.len()
            );
            for f in &verification_failures {
                eprintln!("  - {f}");
            }
        }
        return (
            ExitCode::from(EXIT_VERIFY_MISMATCH),
            serde_json::json!({
                "schema_version": 1,
                "ok": false,
                "verified": verified_ok_count,
                "solutions": solutions_summary,
                "error": {
                    "kind": "VerifyMismatch",
                    "failures": verification_failures
                }
            }),
        );
    }

    if format == OutputFormat::Human {
        println!(
            "All {} solution(s) passed verification!",
            cfg.solutions.len()
        );
    }

    (
        ExitCode::from(EXIT_SUCCESS),
        serde_json::json!({
            "schema_version": 1,
            "ok": true,
            "verified": cfg.solutions.len(),
            "solutions": solutions_summary
        }),
    )
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
    let (config_path, content, cfg) = match load_problem(problem_dir, format) {
        Ok(res) => res,
        Err(code) => return code,
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            emit_error(
                format,
                "DiscoverError",
                format!("Failed to discover tests: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let target_solutions: Vec<_> = cfg
        .solutions
        .iter()
        .filter(|s| s.tag == SolutionTag::Main || s.tag == SolutionTag::Accepted)
        .collect();

    if target_solutions.is_empty() {
        emit_error(
            format,
            "ConfigError",
            "No main or accepted solutions found for calibration",
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = cli.sandbox_mode_str();

    let options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: false,
        precise: false,
    };
    let scheduler = Scheduler::with_session(options, session.clone());

    let mut t_main = 0u64;
    let mut main_slowest_test: Option<String> = None;
    let mut t_slowest_ac = 0u64;
    let mut slowest_ac_test: Option<String> = None;
    let mut peak_mem_kb = 0u64;
    let mut peak_memory_test: Option<String> = None;
    let mut subtask_main_ms: std::collections::BTreeMap<u32, u64> =
        std::collections::BTreeMap::new();
    let mut subtask_slowest_ac_ms: std::collections::BTreeMap<u32, u64> =
        std::collections::BTreeMap::new();
    for st in &cfg.subtasks {
        subtask_main_ms.insert(st.id, 0);
        subtask_slowest_ac_ms.insert(st.id, 0);
    }

    let cached_verify_state: Option<CachedVerifyTlState> = if !cli.no_cache {
        let cache_file = problem_dir
            .join(".uj")
            .join("cache")
            .join("verify_tl_cache.json");
        fs::read(&cache_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedVerifyTlState>(&bytes).ok())
            .filter(|st| st.tests_fingerprint == compute_tests_fingerprint(&tests))
    } else {
        None
    };

    let mut accumulate_report = |sol: &SolutionConfig, report: &SubmissionJudgeReport| {
        for t in &report.tests {
            if t.memory_kb > peak_mem_kb
                || (peak_memory_test.is_none() && t.memory_kb == peak_mem_kb)
            {
                peak_mem_kb = t.memory_kb;
                peak_memory_test = Some(t.id.clone());
            }
            if sol.tag == SolutionTag::Main
                && (t.time_ms > t_main || (main_slowest_test.is_none() && t.time_ms == t_main))
            {
                t_main = t.time_ms;
                main_slowest_test = Some(t.id.clone());
            }
            if t.time_ms > t_slowest_ac || (slowest_ac_test.is_none() && t.time_ms == t_slowest_ac)
            {
                t_slowest_ac = t.time_ms;
                slowest_ac_test = Some(t.id.clone());
            }
            if let Some(st_id) = t.subtask {
                if sol.tag == SolutionTag::Main {
                    let e = subtask_main_ms.entry(st_id).or_insert(0);
                    *e = (*e).max(t.time_ms);
                }
                let e_ac = subtask_slowest_ac_ms.entry(st_id).or_insert(0);
                *e_ac = (*e_ac).max(t.time_ms);
            }
        }
        let max_ms = report.max_time_ms;
        peak_mem_kb = peak_mem_kb.max(report.max_memory_kb);
        if sol.tag == SolutionTag::Main {
            t_main = t_main.max(max_ms);
        }
        t_slowest_ac = t_slowest_ac.max(max_ms);
    };

    let use_tui = cli.tui && format == OutputFormat::Human && std::io::stderr().is_terminal();

    let total_sols = target_solutions.len();
    for (sol_idx, sol) in target_solutions.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return emit_interrupted(format, "Calibration");
        }
        let sol_path = problem_dir.join(&sol.path);
        let compiled = match session.compile_problem_solution(problem_dir, &cfg, &sol_path, false) {
            Ok(c) => c,
            Err(e) => {
                return emit_compile_error(format, Some(&sol_path), e.to_string());
            }
        };

        let path_norm = sol.path.to_string_lossy().replace('\\', "/");
        if let Some(ref state) = cached_verify_state
            && let Ok(bin_hash) = crate::author_steps::hash_file_streaming(&compiled.binary_path)
            && let Some(entry) = state
                .entries
                .iter()
                .find(|e| e.path == path_norm && e.binary_hash == bin_hash)
            && entry.report.verdict.is_accepted()
        {
            accumulate_report(sol, &entry.report);
            continue;
        }

        let sol_name = sol.path.file_name().unwrap_or_default().to_string_lossy();
        let sol_disp = sol.path.display().to_string();
        let sol_num = sol_idx + 1;

        // Run 3 times in exclusive mode (jobs = 1)
        for run_idx in 0..3 {
            if cancel.is_cancelled() {
                return emit_interrupted(format, "Calibration");
            }

            let round_num = run_idx + 1;
            let sub_name = format!("{sol_disp} [calibrating {round_num}/3]");

            let (event_sender, event_thread) = if use_tui {
                let (tx, rx) = crossbeam_channel::unbounded::<uj_report::result::JudgeEvent>();
                let prob_name = cfg.name.clone();
                let cancel_clone = cancel.clone();
                let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
                let cur_sub = sub_name.clone();
                let handle = std::thread::spawn(move || {
                    if let Err(e) = uj_tui::run_tui(
                        &prob_name,
                        &cur_sub,
                        &test_names,
                        1, // 1 exclusive core for calibration
                        rx.clone(),
                        cancel_clone,
                        true,
                    ) {
                        eprintln!(
                            "Warning: Calibration TUI initialization failed ({e}); falling back to non-TUI execution."
                        );
                        while rx.recv().is_ok() {}
                    }
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
                        emit_error(
                            format,
                            "CalibrationError",
                            format!(
                                "Solution '{}' failed with verdict {:?}",
                                sol_name, report.verdict
                            ),
                            None,
                        );
                        return ExitCode::from(EXIT_NON_AC);
                    }
                    accumulate_report(sol, &report);
                }
                Err(e) => {
                    if let Some(h) = event_thread {
                        let _ = h.join();
                    }
                    emit_error(
                        format,
                        "JudgeFailed",
                        format!("Calibration judge failed: {e}"),
                        None,
                    );
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
        let pattern = regex::Regex::new(r#"(?m)^([ \t]*time\s*=\s*)"[^"]*""#).unwrap();
        let updated = pattern.replace(&content, format!(r#"${{1}}"{formatted_tl}""#));
        if let Err(e) = safe_write_file(&config_path, updated.as_bytes()) {
            emit_error(
                format,
                "ConfigWriteError",
                format!("Failed to write updated problem.toml: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    }

    let subtask_stats_json: Vec<serde_json::Value> = subtask_slowest_ac_ms
        .iter()
        .map(|(&st_id, &ac_ms)| {
            let main_ms = subtask_main_ms.get(&st_id).copied().unwrap_or(0);
            serde_json::json!({
                "id": st_id,
                "t_main_ms": main_ms,
                "t_slowest_ac_ms": ac_ms
            })
        })
        .collect();

    let res = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "t_main_ms": t_main,
        "main_slowest_test": main_slowest_test,
        "t_slowest_ac_ms": t_slowest_ac,
        "slowest_ac_test": slowest_ac_test,
        "peak_memory_kb": peak_mem_kb,
        "peak_memory_test": peak_memory_test,
        "recommended_tl_ms": rec_tl_ms,
        "recommended_tl": formatted_tl,
        "subtask_stats": subtask_stats_json,
        "applied": apply
    });

    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => {
            let mem_str = if peak_mem_kb > 1024 {
                format!("{:.1} MiB", peak_mem_kb as f64 / 1024.0)
            } else {
                format!("{} KiB", peak_mem_kb)
            };
            let main_test_suffix = main_slowest_test
                .as_deref()
                .map(|id| format!(" (test {id})"))
                .unwrap_or_default();
            let ac_test_suffix = slowest_ac_test
                .as_deref()
                .map(|id| format!(" (test {id})"))
                .unwrap_or_default();
            let mem_test_suffix = peak_memory_test
                .as_deref()
                .map(|id| format!(" (test {id})"))
                .unwrap_or_default();
            println!("=== CPU Time Limit Calibration ===");
            println!("  Main solution peak CPU:       {t_main} ms{main_test_suffix}");
            println!("  Slowest accepted peak CPU:    {t_slowest_ac} ms{ac_test_suffix}");
            println!("  Peak memory observed:         {mem_str}{mem_test_suffix}");
            if !subtask_slowest_ac_ms.is_empty() {
                println!("  Subtask peak CPU breakdown:");
                for (&st_id, &ac_ms) in &subtask_slowest_ac_ms {
                    let main_ms = subtask_main_ms.get(&st_id).copied().unwrap_or(0);
                    println!("    Subtask #{st_id}: main={main_ms} ms, slowest_ac={ac_ms} ms");
                }
            }
            println!("  Recommended Time Limit:       {formatted_tl} ({rec_tl_ms} ms)");
            if apply {
                println!("  [Applied] Updated limits.time in problem.toml");
            }
        }
        OutputFormat::Silent => {}
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
    gen_args_template: &str,
    budget: Duration,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let (code, _) = stress_core(
        problem_dir,
        gen_src,
        sol_src,
        brute_src,
        count,
        seed_opt,
        gen_args_template,
        budget,
        cli,
        format,
        cancel,
    );
    code
}

#[allow(clippy::too_many_arguments)]
pub fn stress_core(
    problem_dir: &Path,
    gen_src: &str,
    sol_src: &str,
    brute_src: &str,
    count: usize,
    seed_opt: Option<u64>,
    gen_args_template: &str,
    budget: Duration,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> (ExitCode, serde_json::Value) {
    let start_time = Instant::now();

    let gen_path = problem_dir.join(gen_src);
    let sol_path = problem_dir.join(sol_src);
    let brute_path = problem_dir.join(brute_src);

    let config_path = problem_dir.join("problem.toml");
    let cfg_opt = if config_path.exists() {
        let content = match fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Cannot read problem.toml: {e}");
                emit_error(format, "ConfigError", &msg, None);
                return (
                    ExitCode::from(EXIT_USAGE_OR_CONFIG),
                    serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": { "kind": "ConfigError", "message": msg }
                    }),
                );
            }
        };
        match ProblemConfig::from_toml_str(&content, Some(&config_path)) {
            Ok(c) => Some(c),
            Err(e) => {
                let msg = e.to_string();
                let code = handle_config_error_fmt(e, format);
                return (
                    code,
                    serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": { "kind": "ConfigError", "message": msg }
                    }),
                );
            }
        }
    } else {
        None
    };

    let compile_session = CompileSession::new();
    let compiled_gen = match compile_session.compile(&gen_path, cli.no_cache) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Failed to compile generator: {e}");
            let code = emit_compile_error(format, Some(&gen_path), e.to_string());
            return (
                code,
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": { "kind": "CompileError", "message": msg }
                }),
            );
        }
    };
    let compiled_sol = match if let Some(ref cfg) = cfg_opt {
        compile_session.compile_problem_solution(problem_dir, cfg, &sol_path, cli.no_cache)
    } else {
        compile_session.compile(&sol_path, cli.no_cache)
    } {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Failed to compile target solution: {e}");
            let code = emit_compile_error(format, Some(&sol_path), e.to_string());
            return (
                code,
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": { "kind": "CompileError", "message": msg }
                }),
            );
        }
    };
    let compiled_brute = match if let Some(ref cfg) = cfg_opt {
        compile_session.compile_problem_solution(problem_dir, cfg, &brute_path, cli.no_cache)
    } else {
        compile_session.compile(&brute_path, cli.no_cache)
    } {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Failed to compile brute oracle: {e}");
            let code = emit_compile_error(format, Some(&brute_path), e.to_string());
            return (
                code,
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": { "kind": "CompileError", "message": msg }
                }),
            );
        }
    };

    let (resolved_checker, pe_as_wa, sol_limits) = if let Some(ref cfg) = cfg_opt {
        let lim = match cfg.to_limits() {
            Ok(l) => l,
            Err(e) => {
                let msg = e.to_string();
                emit_error(format, "ConfigError", &msg, None);
                return (
                    ExitCode::from(EXIT_USAGE_OR_CONFIG),
                    serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": { "kind": "ConfigError", "message": msg }
                    }),
                );
            }
        };
        let pe = cfg.checker.pe_as_wa;
        let chk = match ProblemContext::prepare(0, problem_dir, cfg, &compile_session) {
            Ok(p_ctx) => p_ctx.resolved_checker.clone(),
            Err(e) => {
                let msg = e.to_string();
                let code = emit_compile_error(format, None, &msg);
                return (
                    code,
                    serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": { "kind": "CompileError", "message": msg }
                    }),
                );
            }
        };
        (chk, pe, lim)
    } else {
        (
            uj_engine::judge::ResolvedChecker::Builtin(BuiltinCheckerType::Wcmp),
            false,
            Limits::new(5000, 512 * 1024 * 1024),
        )
    };

    let gen_bin =
        uj_sandbox::get_or_prepare_authorized_binary(&compiled_gen.binary_path, problem_dir)
            .unwrap_or_else(|_| compiled_gen.binary_path.clone());
    let sol_bin =
        uj_sandbox::get_or_prepare_authorized_binary(&compiled_sol.binary_path, problem_dir)
            .unwrap_or_else(|_| compiled_sol.binary_path.clone());
    let brute_bin =
        uj_sandbox::get_or_prepare_authorized_binary(&compiled_brute.binary_path, problem_dir)
            .unwrap_or_else(|_| compiled_brute.binary_path.clone());

    let topo = uj_engine::topology::Topology::detect(sol_limits.memory_bytes, cli.jobs);
    let num_workers = topo.slots.len().max(1).min(count.max(1));

    let seed_start = seed_opt.unwrap_or(1);
    let seed_counter = Arc::new(AtomicU64::new(seed_start));
    let iter_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let time_budget_exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gen_args_tmpl = gen_args_template.to_string();
    let sandbox_mode = cli.sandbox_mode_str().to_string();
    let require_tier = cli.require_tier;
    let pid = std::process::id();

    enum StressFailure {
        GeneratorFailed {
            seed: u64,
            reason: String,
        },
        BruteFailed {
            seed: u64,
            reason: String,
        },
        ExecutionError {
            message: String,
        },
        SolutionFailed {
            seed: u64,
            reason: String,
            in_bytes: Vec<u8>,
            sol_bytes: Vec<u8>,
        },
        Mismatch {
            seed: u64,
            diff: Option<String>,
            in_bytes: Vec<u8>,
            sol_bytes: Vec<u8>,
            brute_bytes: Vec<u8>,
        },
    }

    let (fail_tx, fail_rx) = crossbeam_channel::unbounded::<StressFailure>();
    let mut handles = Vec::new();

    for worker_idx in 0..num_workers {
        let seed_counter = Arc::clone(&seed_counter);
        let iter_counter = Arc::clone(&iter_counter);
        let stop = Arc::clone(&stop_flag);
        let budget_flag = Arc::clone(&time_budget_exceeded);
        let cancel = cancel.clone();
        let fail_tx = fail_tx.clone();
        let p_dir = problem_dir.to_path_buf();
        let gen_bin = gen_bin.clone();
        let sol_bin = sol_bin.clone();
        let brute_bin = brute_bin.clone();
        let checker = resolved_checker.clone();
        let sol_limits = sol_limits.clone();
        let gen_args_tmpl = gen_args_tmpl.clone();
        let sandbox_mode = sandbox_mode.clone();

        let handle = std::thread::spawn(move || {
            let sandbox = match create_sandbox(&sandbox_mode, require_tier) {
                Ok(s) => s,
                Err(e) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::ExecutionError {
                        message: format!("Sandbox error: {e}"),
                    });
                    return;
                }
            };
            let slot_box = match uj_sandbox::create_slot_box_dir(&p_dir, 5000 + worker_idx) {
                Ok(b) => b,
                Err(e) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::ExecutionError {
                        message: format!("Failed to create sandbox box dir: {e}"),
                    });
                    return;
                }
            };
            let _ = sandbox.authorize_executable(&gen_bin);
            let _ = sandbox.authorize_executable(&sol_bin);
            let _ = sandbox.authorize_executable(&brute_bin);
            match &checker {
                uj_engine::judge::ResolvedChecker::Testlib(chk_exe)
                | uj_engine::judge::ResolvedChecker::Lemon { path: chk_exe, .. } => {
                    let _ = sandbox.authorize_executable(chk_exe);
                }
                uj_engine::judge::ResolvedChecker::Builtin(_) => {}
            }

            let gen_limits = Limits::new(5000, 512 * 1024 * 1024);

            loop {
                if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    break;
                }
                if start_time.elapsed() >= budget {
                    budget_flag.store(true, Ordering::Relaxed);
                    break;
                }

                let prev_iter = iter_counter.fetch_add(1, Ordering::SeqCst);
                if prev_iter >= count {
                    iter_counter.fetch_sub(1, Ordering::SeqCst);
                    break;
                }
                let iter_num = prev_iter + 1;
                let seed = seed_counter.fetch_add(1, Ordering::SeqCst);

                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);

                let rendered_args = gen_args_tmpl
                    .replace("{seed}", &seed.to_string())
                    .replace("{iter}", &iter_num.to_string());
                let mut gen_argv = vec![gen_bin.as_os_str().to_os_string()];
                for arg in rendered_args.split_whitespace() {
                    gen_argv.push(std::ffi::OsString::from(arg));
                }

                // 1. Generate input inside isolated slot box
                let spec_gen = RunSpec {
                    argv: gen_argv,
                    env: vec![(
                        std::ffi::OsString::from("UJ_STRESS"),
                        std::ffi::OsString::from("1"),
                    )],
                    work_dir: slot_box.box_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::Null,
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: gen_limits.clone(),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: true,
                };

                let gen_res = match sandbox.run(&spec_gen, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        stop.store(true, Ordering::Relaxed);
                        let _ = fail_tx.send(StressFailure::ExecutionError {
                            message: format!("Generator failed: {e}"),
                        });
                        break;
                    }
                };

                if !gen_res.status.is_success() || gen_res.killed_by.is_some() || gen_res.oom_killed
                {
                    let stderr_msg = String::from_utf8_lossy(&gen_res.stderr_tail);
                    let reason = format!(
                        "Generator failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                        gen_res.status,
                        gen_res.killed_by,
                        gen_res.oom_killed,
                        stderr_msg.trim()
                    );
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::GeneratorFailed { seed, reason });
                    break;
                }

                let in_bytes = gen_res.stdout.map(|o| o.bytes).unwrap_or_default();
                if in_bytes.is_empty() {
                    let reason = format!(
                        "Generator produced empty stdout on seed {seed} (hint: check if gen.cpp ignores seeds outside fixed test IDs)"
                    );
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::GeneratorFailed { seed, reason });
                    break;
                }
                let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
                let in_tmp = scratch_dir.join(format!("stress_{pid}_{worker_idx}_{seed}.in"));
                let _ = fs::write(&in_tmp, &in_bytes);

                // 2. Run target solution inside isolated slot box
                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                let spec_sol = RunSpec {
                    argv: vec![sol_bin.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: slot_box.box_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(in_tmp.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: sol_limits.clone(),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };

                let sol_res = match sandbox.run(&spec_sol, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = fs::remove_file(&in_tmp);
                        stop.store(true, Ordering::Relaxed);
                        let _ = fail_tx.send(StressFailure::ExecutionError {
                            message: format!("Solution execution failed: {e}"),
                        });
                        break;
                    }
                };

                if !sol_res.status.is_success() || sol_res.killed_by.is_some() || sol_res.oom_killed
                {
                    let _ = fs::remove_file(&in_tmp);
                    let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();
                    let reason = format!(
                        "Solution failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                        sol_res.status,
                        sol_res.killed_by,
                        sol_res.oom_killed,
                        String::from_utf8_lossy(&sol_res.stderr_tail).trim()
                    );
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::SolutionFailed {
                        seed,
                        reason,
                        in_bytes,
                        sol_bytes,
                    });
                    break;
                }

                // 3. Run brute oracle inside isolated slot box
                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                let mut brute_limits = sol_limits.clone();
                brute_limits.cpu_time_ms = brute_limits.cpu_time_ms.saturating_mul(3).max(5000);
                brute_limits.wall_time_ms = brute_limits.wall_time_ms.saturating_mul(3).max(15000);
                let spec_brute = RunSpec {
                    argv: vec![brute_bin.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: slot_box.box_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(in_tmp.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: brute_limits,
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };

                let brute_res = match sandbox.run(&spec_brute, &cancel) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = fs::remove_file(&in_tmp);
                        stop.store(true, Ordering::Relaxed);
                        let _ = fail_tx.send(StressFailure::ExecutionError {
                            message: format!("Brute execution failed: {e}"),
                        });
                        break;
                    }
                };

                if !brute_res.status.is_success()
                    || brute_res.killed_by.is_some()
                    || brute_res.oom_killed
                {
                    let _ = fs::remove_file(&in_tmp);
                    let stderr_msg = String::from_utf8_lossy(&brute_res.stderr_tail);
                    let reason = format!(
                        "Brute oracle failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                        brute_res.status,
                        brute_res.killed_by,
                        brute_res.oom_killed,
                        stderr_msg.trim()
                    );
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::BruteFailed { seed, reason });
                    break;
                }

                let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();
                let brute_bytes = brute_res.stdout.map(|o| o.bytes).unwrap_or_default();

                // 4. Compare using problem's configured checker
                let check = match &checker {
                    uj_engine::judge::ResolvedChecker::Builtin(b) => {
                        let mut cr = b.check(&sol_bytes, &brute_bytes);
                        if pe_as_wa && cr.verdict == Verdict::PresentationError {
                            cr.verdict = Verdict::WrongAnswer;
                        }
                        cr
                    }
                    uj_engine::judge::ResolvedChecker::Testlib(chk_exe) => {
                        let out_tmp =
                            scratch_dir.join(format!("stress_{pid}_{worker_idx}_{seed}.out"));
                        let ans_tmp =
                            scratch_dir.join(format!("stress_{pid}_{worker_idx}_{seed}.ans"));
                        let _ = fs::write(&out_tmp, &sol_bytes);
                        let _ = fs::write(&ans_tmp, &brute_bytes);
                        let cr = uj_checker::testlib::run_testlib_checker(
                            sandbox.as_ref(),
                            chk_exe,
                            &in_tmp,
                            &out_tmp,
                            &ans_tmp,
                            pe_as_wa,
                            &cancel,
                        );
                        let _ = fs::remove_file(&out_tmp);
                        let _ = fs::remove_file(&ans_tmp);
                        cr
                    }
                    uj_engine::judge::ResolvedChecker::Lemon {
                        path: chk_exe,
                        full_score,
                    } => {
                        let out_tmp =
                            scratch_dir.join(format!("stress_{pid}_{worker_idx}_{seed}.out"));
                        let ans_tmp =
                            scratch_dir.join(format!("stress_{pid}_{worker_idx}_{seed}.ans"));
                        let _ = fs::write(&out_tmp, &sol_bytes);
                        let _ = fs::write(&ans_tmp, &brute_bytes);
                        let cr = uj_checker::lemon::run_lemon_checker(
                            sandbox.as_ref(),
                            chk_exe,
                            &in_tmp,
                            &out_tmp,
                            &ans_tmp,
                            *full_score,
                            &cancel,
                        );
                        let _ = fs::remove_file(&out_tmp);
                        let _ = fs::remove_file(&ans_tmp);
                        cr
                    }
                };
                let _ = fs::remove_file(&in_tmp);

                if !check.verdict.is_accepted() {
                    stop.store(true, Ordering::Relaxed);
                    let _ = fail_tx.send(StressFailure::Mismatch {
                        seed,
                        diff: check.message,
                        in_bytes,
                        sol_bytes,
                        brute_bytes,
                    });
                    break;
                }
            }
        });
        handles.push(handle);
    }
    drop(fail_tx);

    let mut failures: Vec<StressFailure> = fail_rx.into_iter().collect();
    for h in handles {
        let _ = h.join();
    }

    if cancel.is_cancelled() {
        let code = emit_interrupted(format, "Stress testing");
        return (
            code,
            serde_json::json!({
                "schema_version": 1,
                "ok": false,
                "error": { "kind": "Interrupted", "message": "Interrupted by user" }
            }),
        );
    }

    let iterations = iter_counter.load(Ordering::SeqCst);

    if !failures.is_empty() {
        // Pick failure with smallest seed for deterministic reporting
        failures.sort_by_key(|f| match f {
            StressFailure::GeneratorFailed { seed, .. }
            | StressFailure::BruteFailed { seed, .. }
            | StressFailure::SolutionFailed { seed, .. }
            | StressFailure::Mismatch { seed, .. } => *seed,
            StressFailure::ExecutionError { .. } => 0,
        });
        match failures.remove(0) {
            StressFailure::ExecutionError { message } => {
                emit_error(format, "ExecutionError", &message, None);
                return (
                    ExitCode::from(EXIT_JUDGE_FAILED),
                    serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": { "kind": "ExecutionError", "message": message }
                    }),
                );
            }
            StressFailure::GeneratorFailed { seed, reason } => {
                let res = serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "seed": seed,
                    "error": {
                        "kind": "GeneratorFailed",
                        "message": reason
                    }
                });
                match format {
                    OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
                    OutputFormat::Human | OutputFormat::Silent => eprintln!("{reason}"),
                }
                return (ExitCode::from(EXIT_JUDGE_FAILED), res);
            }
            StressFailure::BruteFailed { seed, reason } => {
                let res = serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "seed": seed,
                    "error": {
                        "kind": "BruteFailed",
                        "message": reason
                    }
                });
                match format {
                    OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
                    OutputFormat::Human | OutputFormat::Silent => eprintln!("{reason}"),
                }
                return (ExitCode::from(EXIT_JUDGE_FAILED), res);
            }
            StressFailure::SolutionFailed {
                seed,
                reason,
                in_bytes,
                sol_bytes,
            } => {
                let stress_dir = problem_dir.join("stress");
                let _ = fs::create_dir_all(&stress_dir);
                let fail_in = stress_dir.join(format!("fail-{seed}.in"));
                let fail_sol = stress_dir.join(format!("fail-{seed}.sol.out"));
                let _ = safe_write_file(&fail_in, &in_bytes);
                let _ = safe_write_file(&fail_sol, &sol_bytes);
                let res = serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "seed": seed,
                    "diff": reason,
                    "saved_to": stress_dir.to_string_lossy(),
                    "iterations": iterations,
                    "mismatch": {
                        "seed": seed,
                        "input": String::from_utf8_lossy(&in_bytes),
                        "solution_output": String::from_utf8_lossy(&sol_bytes),
                        "message": reason
                    }
                });
                match format {
                    OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
                    OutputFormat::Human => {
                        println!("=== Stress Test Failure Found on Seed {seed} ===");
                        println!("  Message:    {reason}");
                        println!("  Input:      {}", fail_in.display());
                        println!(
                            "  Repro:      uj run -- {} < {}",
                            compiled_sol.binary_path.display(),
                            fail_in.display()
                        );
                    }
                    OutputFormat::Silent => {}
                }
                return (ExitCode::from(EXIT_NON_AC), res);
            }
            StressFailure::Mismatch {
                seed,
                diff,
                in_bytes,
                sol_bytes,
                brute_bytes,
            } => {
                let stress_dir = problem_dir.join("stress");
                let _ = fs::create_dir_all(&stress_dir);
                let fail_in = stress_dir.join(format!("fail-{seed}.in"));
                let fail_sol = stress_dir.join(format!("fail-{seed}.sol.out"));
                let fail_brute = stress_dir.join(format!("fail-{seed}.brute.out"));

                let _ = safe_write_file(&fail_in, &in_bytes);
                let _ = safe_write_file(&fail_sol, &sol_bytes);
                let _ = safe_write_file(&fail_brute, &brute_bytes);

                let res = serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "seed": seed,
                    "diff": diff,
                    "saved_to": stress_dir.to_string_lossy(),
                    "iterations": iterations,
                    "mismatch": {
                        "seed": seed,
                        "input": String::from_utf8_lossy(&in_bytes),
                        "solution_output": String::from_utf8_lossy(&sol_bytes),
                        "brute_output": String::from_utf8_lossy(&brute_bytes),
                        "message": diff
                    }
                });
                match format {
                    OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
                    OutputFormat::Human => {
                        println!("=== Stress Test Mismatch Found on Seed {seed} ===");
                        println!(
                            "  Message:    {}",
                            diff.as_deref().unwrap_or("Output mismatch")
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
                    OutputFormat::Silent => {}
                }
                return (ExitCode::from(EXIT_NON_AC), res);
            }
        }
    }

    let truncated = time_budget_exceeded.load(Ordering::Relaxed) && iterations < count;
    let res = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "iterations": iterations,
        "tested": iterations,
        "truncated": truncated,
        "duration_ms": start_time.elapsed().as_millis(),
        "message": "All stress tests passed"
    });
    match format {
        OutputFormat::Json | OutputFormat::Ndjson => print_structured(format, &res),
        OutputFormat::Human => {
            println!(
                "Stress test completed: {} iterations verified with no mismatches ({:?})",
                iterations,
                start_time.elapsed()
            );
        }
        OutputFormat::Silent => {}
    }

    (ExitCode::from(EXIT_SUCCESS), res)
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
    let (code, _) = pipeline_core(problem_dir, cli, format, cancel);
    code
}

pub fn pipeline_core(
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> (ExitCode, serde_json::Value) {
    uj_engine::clear_answer_cache();
    let session = CompileSession::new();
    let sub_format = match format {
        OutputFormat::Json => OutputFormat::Silent,
        other => other,
    };

    struct PipelineRunner<'a> {
        step_records: Vec<serde_json::Value>,
        failed_step: Option<String>,
        failure_detail: Option<serde_json::Value>,
        final_code: ExitCode,
        cancel: &'a CancelToken,
        format: OutputFormat,
    }

    impl<'a> PipelineRunner<'a> {
        fn new(cancel: &'a CancelToken, format: OutputFormat) -> Self {
            Self {
                step_records: Vec::new(),
                failed_step: None,
                failure_detail: None,
                final_code: ExitCode::from(EXIT_SUCCESS),
                cancel,
                format,
            }
        }

        fn finish_stage(&mut self, name: &'static str, status: &'static str) {
            self.step_records.push(serde_json::json!({
                "name": name,
                "status": status,
            }));
            if self.format == OutputFormat::Ndjson {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "step_finished",
                        "step": name,
                        "status": status
                    })
                );
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
            self.finish_stage(name, if success { "success" } else { "failed" });

            if !success {
                self.failed_step = Some(name.to_string());
                self.final_code = code;
                if self.failure_detail.is_none() {
                    self.failure_detail = Some(serde_json::json!({
                        "stage": name,
                        "message": format!("Step 'uj {name}' failed")
                    }));
                }
                if self.format == OutputFormat::Human {
                    eprintln!("Pipeline stopped: step 'uj {name}' failed with status {code:?}");
                }
                return false;
            }
            true
        }
    }

    let mut runner = PipelineRunner::new(cancel, format);

    let use_staged = std::env::var("UJ_PIPELINE_STAGED").as_deref() == Ok("1");
    if use_staged {
        if runner.step("build", || {
            run_build_with_session(problem_dir, &[], cli.no_cache, sub_format, &session)
        }) && runner.step("gen", || {
            run_gen_with_session(problem_dir, false, false, cli, sub_format, cancel, &session)
        }) && runner.step("validate", || {
            run_validate_with_session(problem_dir, cli, sub_format, cancel, &session)
        }) && runner.step("ans", || {
            run_ans_with_session(problem_dir, false, cli, sub_format, cancel, &session)
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
                runner.finish_stage("gen", "success");
                runner.finish_stage("validate", "success");
                runner.finish_stage("ans", "success");

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
                test_name,
                message,
                exit_code,
            } => {
                runner.final_code = exit_code;
                let s_name = stage.name();
                runner.failed_step = Some(s_name.to_string());
                runner.failure_detail = Some(serde_json::json!({
                    "stage": s_name,
                    "test": test_name,
                    "message": message,
                }));
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.finish_stage("gen", "failed");
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.finish_stage("gen", "aborted");
                        runner.finish_stage("validate", "failed");
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.finish_stage("gen", "aborted");
                        runner.finish_stage("validate", "aborted");
                        runner.finish_stage("ans", "failed");
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
            crate::author_steps::CaseStagesOutcome::RuntimeError { stage, message } => {
                runner.final_code = ExitCode::from(EXIT_JUDGE_FAILED);
                let s_name = stage.name();
                runner.failed_step = Some(s_name.to_string());
                runner.failure_detail = Some(serde_json::json!({
                    "stage": s_name,
                    "message": message,
                }));
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.finish_stage("gen", "failed");
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.finish_stage("gen", "aborted");
                        runner.finish_stage("validate", "failed");
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.finish_stage("gen", "aborted");
                        runner.finish_stage("validate", "aborted");
                        runner.finish_stage("ans", "failed");
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
            crate::author_steps::CaseStagesOutcome::ConfigOrCompileError {
                stage,
                message,
                exit_code,
            } => {
                runner.final_code = exit_code;
                let s_name = stage.name();
                runner.failed_step = Some(s_name.to_string());
                runner.failure_detail = Some(serde_json::json!({
                    "stage": s_name,
                    "message": message,
                }));
                match stage {
                    uj_tui::pipeline::Stage::Gen => {
                        runner.finish_stage("gen", "failed");
                    }
                    uj_tui::pipeline::Stage::Validate => {
                        runner.finish_stage("gen", "success");
                        runner.finish_stage("validate", "failed");
                    }
                    uj_tui::pipeline::Stage::Ans => {
                        runner.finish_stage("gen", "success");
                        runner.finish_stage("validate", "success");
                        runner.finish_stage("ans", "failed");
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
                runner.failed_step = Some("interrupted".to_string());
                runner.failure_detail = Some(serde_json::json!({
                    "kind": "Interrupted",
                    "message": "Interrupted by user"
                }));
            }
        }
    }

    let is_ok = runner.failed_step.is_none() && runner.final_code == ExitCode::from(EXIT_SUCCESS);
    let res = serde_json::json!({
        "schema_version": 1,
        "ok": is_ok,
        "steps": runner.step_records,
        "failed_step": runner.failed_step,
        "error": runner.failure_detail
    });

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else if format == OutputFormat::Ndjson {
        println!(
            "{}",
            serde_json::json!({
                "event": "pipeline_finished",
                "ok": is_ok,
                "failed_step": res["failed_step"],
                "error": res["error"]
            })
        );
    } else if format == OutputFormat::Human && is_ok {
        println!("Pipeline completed successfully!");
    }

    (runner.final_code, res)
}

// ============================================================================
// 10. uj pack
// ============================================================================

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
fn crc32_ieee(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFFu32, data)
}

#[cfg(test)]
pub(crate) fn build_flat_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut central_dir = Vec::new();

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32_ieee(data);
        let size = data.len() as u32;
        let offset = buf.len() as u32;

        // Local file header (30 bytes + filename)
        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // signature
        buf.extend_from_slice(&20u16.to_le_bytes()); // version needed (2.0)
        buf.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename flag
        buf.extend_from_slice(&0u16.to_le_bytes()); // compression method: 0 (Store)
        buf.extend_from_slice(&0u16.to_le_bytes()); // last mod time
        buf.extend_from_slice(&0x0021u16.to_le_bytes()); // last mod date (1980-01-01)
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes()); // compressed size
        buf.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(data);

        // Central directory header (46 bytes + filename)
        central_dir.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // signature
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central_dir.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 flag
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // method: Store
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central_dir.extend_from_slice(&0x0021u16.to_le_bytes()); // mod date
        central_dir.extend_from_slice(&crc.to_le_bytes());
        central_dir.extend_from_slice(&size.to_le_bytes());
        central_dir.extend_from_slice(&size.to_le_bytes());
        central_dir.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central_dir.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central_dir.extend_from_slice(&offset.to_le_bytes()); // local header offset
        central_dir.extend_from_slice(name_bytes);
    }

    let cd_offset = buf.len() as u32;
    let cd_size = central_dir.len() as u32;
    let entry_count = entries.len() as u16;

    buf.extend_from_slice(&central_dir);

    // End of central directory record (22 bytes)
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD signature
    buf.extend_from_slice(&0u16.to_le_bytes()); // number of this disk
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk where CD starts
    buf.extend_from_slice(&entry_count.to_le_bytes());
    buf.extend_from_slice(&entry_count.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment length

    buf
}

fn write_flat_zip_streaming(entries: &[(String, PathBuf)], out_path: &Path) -> Result<u64, String> {
    use std::io::{Read, Write};

    let entry_count = u16::try_from(entries.len()).map_err(|_| {
        format!(
            "Too many files to pack into standard ZIP ({} > 65535)",
            entries.len()
        )
    })?;

    let parent = out_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("Failed to create output directory: {e}"))?;

    let file_name = out_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.zip");
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}.{nanos}"));

    let result = (|| -> Result<u64, String> {
        let out_file = fs::File::create(&tmp_path).map_err(|e| {
            format!(
                "Failed to create temporary archive '{}': {e}",
                tmp_path.display()
            )
        })?;
        let mut writer = std::io::BufWriter::new(out_file);
        let mut central_dir: Vec<u8> = Vec::with_capacity(entries.len() * 64);
        let mut offset: u64 = 0;
        let mut chunk = vec![0u8; 64 * 1024];

        for (name, src_path) in entries {
            let name_bytes = name.as_bytes();
            let name_len = u16::try_from(name_bytes.len())
                .map_err(|_| format!("Entry filename '{name}' exceeds 65535 bytes"))?;
            let local_offset = u32::try_from(offset)
                .map_err(|_| "Archive size exceeds 4 GiB ZIP32 offset limit".to_string())?;

            // Pass 1: compute CRC32 and exact byte size in streaming chunks
            let mut crc_state = 0xFFFF_FFFFu32;
            let mut file_size: u64 = 0;
            {
                let mut f = fs::File::open(src_path)
                    .map_err(|e| format!("Failed to read '{}': {e}", src_path.display()))?;
                loop {
                    let n = f
                        .read(&mut chunk)
                        .map_err(|e| format!("Failed to read '{}': {e}", src_path.display()))?;
                    if n == 0 {
                        break;
                    }
                    crc_state = crc32_update(crc_state, &chunk[..n]);
                    file_size += n as u64;
                }
            }
            let crc = !crc_state;
            let size32 = u32::try_from(file_size).map_err(|_| {
                format!(
                    "File '{}' ({} bytes) exceeds 4 GiB ZIP32 limit",
                    src_path.display(),
                    file_size
                )
            })?;

            // Write local file header (30 bytes + filename)
            let mut local_hdr = Vec::with_capacity(30 + name_bytes.len());
            local_hdr.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local_hdr.extend_from_slice(&20u16.to_le_bytes());
            local_hdr.extend_from_slice(&0x0800u16.to_le_bytes());
            local_hdr.extend_from_slice(&0u16.to_le_bytes());
            local_hdr.extend_from_slice(&0u16.to_le_bytes());
            local_hdr.extend_from_slice(&0x0021u16.to_le_bytes());
            local_hdr.extend_from_slice(&crc.to_le_bytes());
            local_hdr.extend_from_slice(&size32.to_le_bytes());
            local_hdr.extend_from_slice(&size32.to_le_bytes());
            local_hdr.extend_from_slice(&name_len.to_le_bytes());
            local_hdr.extend_from_slice(&0u16.to_le_bytes());
            local_hdr.extend_from_slice(name_bytes);
            writer
                .write_all(&local_hdr)
                .map_err(|e| format!("Failed to write archive header: {e}"))?;

            // Pass 2: stream file bytes directly to writer
            let mut written_file_bytes: u64 = 0;
            {
                let mut f = fs::File::open(src_path)
                    .map_err(|e| format!("Failed to read '{}': {e}", src_path.display()))?;
                loop {
                    let n = f
                        .read(&mut chunk)
                        .map_err(|e| format!("Failed to read '{}': {e}", src_path.display()))?;
                    if n == 0 {
                        break;
                    }
                    writer
                        .write_all(&chunk[..n])
                        .map_err(|e| format!("Failed to write archive data: {e}"))?;
                    written_file_bytes += n as u64;
                }
            }
            if written_file_bytes != file_size {
                return Err(format!(
                    "File '{}' changed size while packing",
                    src_path.display()
                ));
            }

            offset += local_hdr.len() as u64 + file_size;

            // Central directory header (46 bytes + filename)
            central_dir.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central_dir.extend_from_slice(&20u16.to_le_bytes());
            central_dir.extend_from_slice(&20u16.to_le_bytes());
            central_dir.extend_from_slice(&0x0800u16.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0x0021u16.to_le_bytes());
            central_dir.extend_from_slice(&crc.to_le_bytes());
            central_dir.extend_from_slice(&size32.to_le_bytes());
            central_dir.extend_from_slice(&size32.to_le_bytes());
            central_dir.extend_from_slice(&name_len.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0u16.to_le_bytes());
            central_dir.extend_from_slice(&0u32.to_le_bytes());
            central_dir.extend_from_slice(&local_offset.to_le_bytes());
            central_dir.extend_from_slice(name_bytes);
        }

        let cd_offset = u32::try_from(offset)
            .map_err(|_| "Archive size exceeds 4 GiB ZIP32 offset limit".to_string())?;
        let cd_size = u32::try_from(central_dir.len())
            .map_err(|_| "Central directory exceeds 4 GiB ZIP32 limit".to_string())?;

        writer
            .write_all(&central_dir)
            .map_err(|e| format!("Failed to write central directory: {e}"))?;

        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&entry_count.to_le_bytes());
        eocd.extend_from_slice(&entry_count.to_le_bytes());
        eocd.extend_from_slice(&cd_size.to_le_bytes());
        eocd.extend_from_slice(&cd_offset.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        writer
            .write_all(&eocd)
            .map_err(|e| format!("Failed to write EOCD record: {e}"))?;

        writer
            .flush()
            .map_err(|e| format!("Failed to flush archive: {e}"))?;
        let inner = writer
            .into_inner()
            .map_err(|e| format!("Failed to finalize archive buffer: {e}"))?;
        let _ = inner.sync_all();
        drop(inner);

        if let Err(first_err) = fs::rename(&tmp_path, out_path) {
            if out_path.exists() {
                let _ = fs::remove_file(out_path);
            }
            fs::rename(&tmp_path, out_path).map_err(|_| {
                format!(
                    "Failed to move temporary archive into '{}': {first_err}",
                    out_path.display()
                )
            })?;
        }

        Ok(offset + central_dir.len() as u64 + 22)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

pub fn run_pack(problem_dir: &Path, output: &Path, clean: bool, format: OutputFormat) -> ExitCode {
    let (_, _, cfg) = match load_problem(problem_dir, format) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let discovered = match uj_core::tests::discover_tests(problem_dir, &cfg) {
        Ok(t) => t,
        Err(e) => {
            emit_error(
                format,
                "DiscoverError",
                format!("Failed to discover tests: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let total_gen_count: u64 = cfg.tests.generators.iter().map(|g| g.count as u64).sum();
    let non_wildcard_patterns: Vec<String> = cfg
        .subtasks
        .iter()
        .flat_map(|s| s.tests.iter().filter(|p| p.trim() != "*").cloned())
        .collect();

    let valid_tests: Vec<_> = discovered
        .into_iter()
        .filter(|tc| {
            if !cfg.subtasks.is_empty() && tc.subtask_id.is_none() {
                return false;
            }
            if total_gen_count > 0
                && let Ok(num) = tc.name.parse::<u64>()
                && num > total_gen_count
                && !uj_core::tests::test_matches_patterns(&tc.name, &non_wildcard_patterns)
            {
                return false;
            }
            true
        })
        .collect();

    if valid_tests.is_empty() {
        emit_error(
            format,
            "EmptyTests",
            "No valid test cases found to pack",
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let tests_dir = problem_dir.join(&cfg.tests.dir);
    let mut zip_entries: Vec<(String, PathBuf)> = Vec::with_capacity(valid_tests.len() * 2);
    for tc in &valid_tests {
        let in_path = tc.input_path.clone();
        let ans_path = tests_dir.join(format!("{}.ans", tc.name));
        if !ans_path.is_file() {
            let msg = format!(
                "Missing answer file for test '{}' (expected {}; run 'uj ans' first)",
                tc.name,
                ans_path.display()
            );
            match format {
                OutputFormat::Json | OutputFormat::Ndjson => {
                    let err = serde_json::json!({
                        "schema_version": 1,
                        "ok": false,
                        "error": {
                            "kind": "MissingAnswerFile",
                            "test": tc.name,
                            "message": msg
                        }
                    });
                    print_structured(format, &err);
                }
                OutputFormat::Human | OutputFormat::Silent => {
                    eprintln!("{msg}");
                }
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        zip_entries.push((format!("{}.in", tc.name), in_path));
        zip_entries.push((format!("{}.ans", tc.name), ans_path));
    }

    let mut cleaned_files: Vec<String> = Vec::new();
    if clean && tests_dir.is_dir() {
        let raw_dir_str = cfg.tests.dir.to_string_lossy();
        let raw_dir = raw_dir_str.trim();
        let same_as_root = raw_dir.is_empty()
            || raw_dir == "."
            || match (fs::canonicalize(&tests_dir), fs::canonicalize(problem_dir)) {
                (Ok(t_canon), Ok(p_canon)) => t_canon == p_canon,
                _ => tests_dir == problem_dir,
            };
        if same_as_root {
            emit_error(
                format,
                "UnsafeCleanDir",
                "Refusing to run 'uj pack --clean' when tests.dir resolves to the problem root directory",
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        let mut expected_names = std::collections::HashSet::new();
        for tc in &valid_tests {
            expected_names.insert(format!("{}.in", tc.name));
            expected_names.insert(format!("{}.ans", tc.name));
        }
        if let Ok(entries) = fs::read_dir(&tests_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_test_artifact =
                    path.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| {
                            matches!(ext.to_ascii_lowercase().as_str(), "in" | "ans" | "out")
                        });
                if path.is_file()
                    && is_test_artifact
                    && let Some(fname) = path.file_name().and_then(|n| n.to_str())
                    && !expected_names.contains(fname)
                    && fs::remove_file(&path).is_ok()
                {
                    cleaned_files.push(fname.to_string());
                }
            }
        }
        cleaned_files.sort_by(|a, b| uj_core::tests::natural_cmp(a, b));
    }

    let out_path = if output.is_absolute() {
        output.to_path_buf()
    } else {
        problem_dir.join(output)
    };

    let size_bytes = match write_flat_zip_streaming(&zip_entries, &out_path) {
        Ok(sz) => sz,
        Err(msg) => {
            emit_error(format, "IoError", &msg, None);
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    match format {
        OutputFormat::Json | OutputFormat::Ndjson => {
            let res = serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "output": output.to_string_lossy(),
                "output_path": out_path.to_string_lossy(),
                "test_count": valid_tests.len(),
                "size_bytes": size_bytes,
                "cleaned_files": cleaned_files
            });
            print_structured(format, &res);
        }
        OutputFormat::Human => {
            if !cleaned_files.is_empty() {
                println!(
                    "Cleaned {} orphan file(s) from {}:",
                    cleaned_files.len(),
                    tests_dir.display()
                );
                for f in &cleaned_files {
                    println!("  - {f}");
                }
            }
            println!(
                "Packed {} test cases ({} files, {} bytes) -> {}",
                valid_tests.len(),
                zip_entries.len(),
                size_bytes,
                out_path.display()
            );
        }
        OutputFormat::Silent => {}
    }

    ExitCode::from(EXIT_SUCCESS)
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
                    time_ms: 10 + (i as u64) * 5,
                    wall_ms: 10 + (i as u64) * 5,
                    memory_kb: 1024 + (i as u64) * 128,
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
            allow_verdicts: None,
        };
        let sol_ac = SolutionConfig {
            path: PathBuf::from("ac.cpp"),
            tag: SolutionTag::Accepted,
            expect_subtasks: None,
            allow_verdicts: None,
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
            allow_verdicts: None,
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
    fn test_tag_wrong_answer_or_time_limit_and_allow_verdicts() {
        let cfg = make_cfg();
        let sol_wa_or_tle = SolutionConfig {
            path: PathBuf::from("wrong_greedy.cpp"),
            tag: SolutionTag::WrongAnswerOrTimeLimit,
            expect_subtasks: None,
            allow_verdicts: None,
        };

        // Mixed WA + TLE passes for WrongAnswerOrTimeLimit
        let rep_mixed = make_report(
            Verdict::TimeLimitExceeded,
            20.0,
            vec![
                Verdict::Accepted,
                Verdict::WrongAnswer,
                Verdict::TimeLimitExceeded,
            ],
            vec![],
        );
        assert!(check_solution_tag(&sol_wa_or_tle, &rep_mixed, &cfg).is_empty());

        // Pure WA also passes for WrongAnswerOrTimeLimit
        let rep_pure_wa = make_report(
            Verdict::WrongAnswer,
            20.0,
            vec![Verdict::Accepted, Verdict::WrongAnswer],
            vec![],
        );
        assert!(check_solution_tag(&sol_wa_or_tle, &rep_pure_wa, &cfg).is_empty());

        // Pure TLE without any WA fails for WrongAnswerOrTimeLimit (requires at least one WA)
        let rep_pure_tle = make_report(
            Verdict::TimeLimitExceeded,
            20.0,
            vec![Verdict::Accepted, Verdict::TimeLimitExceeded],
            vec![],
        );
        assert!(!check_solution_tag(&sol_wa_or_tle, &rep_pure_tle, &cfg).is_empty());

        // Unexpected RE fails for WrongAnswerOrTimeLimit
        let rep_with_re = make_report(
            Verdict::RuntimeError,
            0.0,
            vec![Verdict::WrongAnswer, Verdict::RuntimeError],
            vec![],
        );
        assert!(!check_solution_tag(&sol_wa_or_tle, &rep_with_re, &cfg).is_empty());

        // Custom allow_verdicts on tag = "wa" allowing TLE
        let sol_custom_allow = SolutionConfig {
            path: PathBuf::from("custom_wa_tle.cpp"),
            tag: SolutionTag::WrongAnswer,
            expect_subtasks: None,
            allow_verdicts: Some(vec![Verdict::WrongAnswer, Verdict::TimeLimitExceeded]),
        };
        assert!(check_solution_tag(&sol_custom_allow, &rep_mixed, &cfg).is_empty());
        assert!(!check_solution_tag(&sol_custom_allow, &rep_with_re, &cfg).is_empty());
    }

    #[test]
    fn test_summarize_solution_report() {
        let sol = SolutionConfig {
            path: PathBuf::from("solutions/tle.cpp"),
            tag: SolutionTag::TimeLimit,
            expect_subtasks: None,
            allow_verdicts: None,
        };
        let rep = make_report(
            Verdict::TimeLimitExceeded,
            50.0,
            vec![
                Verdict::Accepted,
                Verdict::TimeLimitExceeded,
                Verdict::Accepted,
            ],
            vec![],
        );
        let summary = summarize_solution_report(&sol, &rep, true);
        assert_eq!(summary["path"], "solutions/tle.cpp");
        assert_eq!(summary["tag"], "time_limit");
        assert_eq!(summary["matches_tag"], true);
        assert_eq!(summary["verdict"], "TLE");
        assert_eq!(summary["max_time_test"], "3");
        assert_eq!(summary["first_fail_test"], "2");
        assert_eq!(summary["first_fail_verdict"], "TLE");
    }

    #[test]
    fn test_build_flat_zip_signature_and_entries() {
        let entries = vec![
            ("1.in".to_string(), b"1 2\n".to_vec()),
            ("1.ans".to_string(), b"3\n".to_vec()),
        ];
        let zip_bytes = build_flat_zip(&entries);
        // Starts with PK\x03\x04 local file header signature
        assert_eq!(&zip_bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        // Ends with EOCD record (22 bytes starting with PK\x05\x06)
        let eocd_start = zip_bytes.len() - 22;
        assert_eq!(
            &zip_bytes[eocd_start..eocd_start + 4],
            &[0x50, 0x4b, 0x05, 0x06]
        );
    }

    #[test]
    fn test_tag_time_limit() {
        let cfg = make_cfg();
        let sol = SolutionConfig {
            path: PathBuf::from("tle.cpp"),
            tag: SolutionTag::TimeLimit,
            expect_subtasks: None,
            allow_verdicts: None,
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
            allow_verdicts: None,
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
            allow_verdicts: None,
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
            allow_verdicts: None,
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
            allow_verdicts: None,
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
            allow_verdicts: None,
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

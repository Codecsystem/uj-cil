//! Authoring workflow commands: init, build, gen, validate, ans, verify, tl, stress, pipeline.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use uj_checker::builtin::BuiltinCheckerType;
use uj_core::config::{ProblemConfig, SolutionTag};
use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_engine::compiler::Compiler;
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_sandbox::{CancelToken, create_sandbox};

use crate::cli::{Cli, OutputFormat, SandboxMode};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS,
    EXIT_USAGE_OR_CONFIG, EXIT_VERIFY_MISMATCH, handle_config_error,
};

const VENDORED_TESTLIB_H: &str = include_str!("../../../assets/testlib.h");

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

[limits]
time = "1.0s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

validator = "validator.cpp"

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
    } else {
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

    for target in to_compile {
        if target
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        {
            continue;
        }
        match Compiler::compile(&target, no_cache) {
            Ok(c) => {
                if format == OutputFormat::Human {
                    println!("Compiled {} ({} ms)", target.display(), c.duration_ms);
                }
            }
            Err(e) => {
                if format == OutputFormat::Json {
                    let err = serde_json::json!({
                        "ok": false,
                        "error": {
                            "kind": "CompileError",
                            "target": target.to_string_lossy(),
                            "message": e.to_string()
                        }
                    });
                    println!("{}", serde_json::to_string_pretty(&err).unwrap());
                } else {
                    eprintln!("Failed to compile {}: {e}", target.display());
                }
                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        }
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
    format: OutputFormat,
    cancel: &CancelToken,
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

    // Find starting test index
    let mut max_idx = 0u32;
    if let Ok(entries) = fs::read_dir(&tests_dir) {
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

    let sandbox = match create_sandbox("auto", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Sandbox error: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let mut generated_count = 0;
    for gen_spec in &cfg.tests.generators {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }
        let gen_path = problem_dir.join(&gen_spec.generator);
        let compiled_gen = match Compiler::compile(&gen_path, false) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to compile generator '{}': {e}", gen_path.display());
                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        };

        for i in 0..gen_spec.count {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }
            max_idx += 1;
            let target_in = tests_dir.join(format!("{max_idx}.in"));

            // Blake3 cache check
            let mut cache_hasher = blake3::Hasher::new();
            if let Ok(b) = fs::read(&compiled_gen.binary_path) {
                cache_hasher.update(&b);
            }
            cache_hasher.update(gen_spec.args.as_bytes());
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

            let limits = Limits::new(5000, 512 * 1024 * 1024);
            let spec = RunSpec {
                argv,
                env: vec![],
                work_dir: tests_dir.clone(),
                ro_binds: vec![],
                stdin: Stdio::Null,
                stdout: Stdio::Pipe,
                stderr: Stdio::Pipe,
                extra_fds: vec![],
                limits,
                cpu_affinity: None,
                seccomp_profile: Default::default(),
            };

            let res = match sandbox.run(&spec, cancel) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Generator execution error: {e}");
                    return ExitCode::from(EXIT_JUDGE_FAILED);
                }
            };

            if !res.status.is_success() {
                let err_tail = String::from_utf8_lossy(&res.stderr_tail);
                eprintln!(
                    "Generator '{}' failed with args '{}': {err_tail}",
                    gen_spec.generator, gen_spec.args
                );
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }

            let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
            let _ = fs::write(&target_in, stdout_bytes);
            let _ = fs::create_dir_all(cache_meta.parent().unwrap());
            let _ = fs::write(cache_meta, &hash);
            generated_count += 1;
        }
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "generated": generated_count });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
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
    let compiled_val = match Compiler::compile(&val_path, false) {
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

    let sandbox = match create_sandbox("auto", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Sandbox error: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    for test in &tests {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let mut argv = vec![compiled_val.binary_path.as_os_str().to_os_string()];
        if let Some(st_id) = test.subtask_id {
            argv.push(std::ffi::OsString::from("--group"));
            argv.push(std::ffi::OsString::from(format!("{st_id}")));
        }

        let limits = Limits::new(5000, 512 * 1024 * 1024);
        let spec = RunSpec {
            argv,
            env: vec![],
            work_dir: problem_dir.to_path_buf(),
            ro_binds: vec![],
            stdin: Stdio::File(test.input_path.clone()),
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits,
            cpu_affinity: None,
            seccomp_profile: Default::default(),
        };

        let res = match sandbox.run(&spec, cancel) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Validator runtime error: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        if !res.status.is_success() {
            let msg = String::from_utf8_lossy(&res.stderr_tail).trim().to_string();
            if format == OutputFormat::Json {
                let err = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "ValidationError",
                        "test": test.name,
                        "message": msg
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err).unwrap());
            } else {
                eprintln!("Validator failed on test '{}': {msg}", test.name);
            }
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "validated": tests.len() });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
        println!("All {} test case(s) successfully validated", tests.len());
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
    let compiled = match Compiler::compile(&sol_path, false) {
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

    let sandbox = match create_sandbox("auto", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Sandbox error: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let main_bytes = fs::read(&compiled.binary_path).unwrap_or_default();
    let mut main_hasher = blake3::Hasher::new();
    main_hasher.update(&main_bytes);
    let main_hash = main_hasher.finalize().to_hex().to_string();

    let mut generated_count = 0;
    for test in &tests {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let in_bytes = fs::read(&test.input_path).unwrap_or_default();
        let in_hash = blake3::hash(&in_bytes).to_hex().to_string();

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

        let limits = cfg
            .to_limits()
            .unwrap_or_else(|_| Limits::new(1000, 256 * 1024 * 1024));
        let spec = RunSpec {
            argv: vec![compiled.binary_path.as_os_str().to_os_string()],
            env: vec![],
            work_dir: problem_dir.to_path_buf(),
            ro_binds: vec![],
            stdin: Stdio::File(test.input_path.clone()),
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits,
            cpu_affinity: None,
            seccomp_profile: Default::default(),
        };

        let res = match sandbox.run(&spec, cancel) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Execution error: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

        if !res.status.is_success() || res.killed_by.is_some() {
            let reason = res
                .killed_by
                .map(|k| format!("{k:?}"))
                .unwrap_or_else(|| format!("{:?}", res.status));
            eprintln!("Main solution failed on test '{}': {reason}", test.name);
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        let stdout_bytes = res.stdout.map(|o| o.bytes).unwrap_or_default();
        let _ = fs::write(&ans_path, &stdout_bytes);
        let _ = fs::create_dir_all(cache_meta.parent().unwrap());
        let _ = fs::write(&cache_meta, &stdout_bytes);
        generated_count += 1;
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({ "ok": true, "answers_generated": generated_count });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
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

    let options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: cli.fail_fast,
        precise: cli.precise,
    };
    let scheduler = Scheduler::new(options);

    let mut verification_failures = Vec::new();

    for sol in &cfg.solutions {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        let sol_path = problem_dir.join(&sol.path);
        let compiled = match Compiler::compile(&sol_path, cli.no_cache) {
            Ok(c) => c,
            Err(e) => {
                verification_failures
                    .push(format!("{}: compilation failed: {e}", sol.path.display()));
                continue;
            }
        };

        let report = match scheduler.judge_submission(
            problem_dir,
            &cfg,
            &compiled,
            tests.clone(),
            cli.jobs,
            None,
            cancel,
        ) {
            Ok(r) => r,
            Err(e) => {
                verification_failures.push(format!("{}: judging failed: {e}", sol.path.display()));
                continue;
            }
        };

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
                    )
                });
                if !has_re || has_other {
                    matches_tag = false;
                }
            }
            SolutionTag::Rejected => {
                if report.verdict == Verdict::Accepted {
                    matches_tag = false;
                }
            }
            SolutionTag::Partial => {
                if report.score <= 0.0 || report.score >= 100.0 {
                    matches_tag = false;
                }
            }
        }

        if !matches_tag {
            verification_failures.push(format!(
                "{} expected tag '{:?}' but got verdict '{:?}' (score: {})",
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
    } else {
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
    let scheduler = Scheduler::new(options);

    let mut t_main = 0u64;
    let mut t_slowest_ac = 0u64;

    for sol in target_solutions {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }
        let sol_path = problem_dir.join(&sol.path);
        let compiled = match Compiler::compile(&sol_path, false) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to compile solution: {e}");
                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        };

        // Run 3 times in exclusive mode (jobs = 1)
        for _ in 0..3 {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }
            if let Ok(report) = scheduler.judge_submission(
                problem_dir,
                &cfg,
                &compiled,
                tests.clone(),
                Some(1),
                None,
                cancel,
            ) {
                let max_ms = report.max_time_ms;
                if sol.tag == SolutionTag::Main {
                    t_main = t_main.max(max_ms);
                }
                t_slowest_ac = t_slowest_ac.max(max_ms);
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
        let _ = fs::write(&config_path, updated.as_bytes());
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({
            "ok": true,
            "t_main_ms": t_main,
            "t_slowest_ac_ms": t_slowest_ac,
            "recommended_tl_ms": rec_tl_ms,
            "recommended_tl": formatted_tl,
            "applied": apply
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
        println!("=== CPU Time Limit Calibration ===");
        println!("  Main solution peak CPU:       {t_main} ms");
        println!("  Slowest accepted peak CPU:    {t_slowest_ac} ms");
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

    let compiled_gen = match Compiler::compile(&gen_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile generator: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };
    let compiled_sol = match Compiler::compile(&sol_path, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to compile target solution: {e}");
            return ExitCode::from(EXIT_COMPILE_ERROR);
        }
    };
    let compiled_brute = match Compiler::compile(&brute_path, false) {
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
        };

        let gen_res = match sandbox.run(&spec_gen, cancel) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Generator failed: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

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
        };

        let sol_res = match sandbox.run(&spec_sol, cancel) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&in_tmp);
                eprintln!("Solution execution failed: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        };

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
    macro_rules! step {
        ($name:expr, $expr:expr) => {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }
            if format == OutputFormat::Human {
                println!("==> Step: uj {}", $name);
            }
            let code = $expr;
            if code != ExitCode::from(EXIT_SUCCESS) {
                if format == OutputFormat::Human {
                    eprintln!(
                        "Pipeline stopped: step 'uj {}' failed with status {:?}",
                        $name, code
                    );
                }
                return code;
            }
        };
    }

    step!("build", run_build(problem_dir, &[], cli.no_cache, format));
    step!("gen", run_gen(problem_dir, false, format, cancel));
    step!("validate", run_validate(problem_dir, format, cancel));
    step!("ans", run_ans(problem_dir, false, format, cancel));
    step!("verify", run_verify(problem_dir, cli, format, cancel));
    step!("tl", run_tl(problem_dir, false, cli, format, cancel));

    if format == OutputFormat::Human {
        println!("Pipeline completed successfully!");
    }
    ExitCode::from(EXIT_SUCCESS)
}

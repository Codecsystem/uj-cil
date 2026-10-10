//! CLI handlers for judge, last, show, and clean commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::SystemTime;

use crossbeam_channel::unbounded;
use uj_core::config::ProblemConfig;
use uj_core::verdict::Verdict;
use uj_engine::compiler::{CompileError, Compiler};
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_report::human::{format_human_report, format_test_details};
use uj_report::result::{JudgeEvent, SubmissionJudgeReport};
use uj_sandbox::CancelToken;

use crate::cli::{Cli, OutputFormat, SandboxMode};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS,
    EXIT_USAGE_OR_CONFIG, handle_config_error,
};

/// Save a report to .uj/runs and update .uj/runs/last.json
fn save_run_report(problem_dir: &Path, report: &SubmissionJudgeReport) {
    let Ok(json) = serde_json::to_string_pretty(report) else {
        return;
    };

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let run_filename = format!("run-{ts}.json");

    let targets = [
        problem_dir.join(".uj").join("runs"),
        PathBuf::from(".uj").join("runs"),
    ];
    for runs_dir in targets {
        let _ = fs::create_dir_all(&runs_dir);
        let _ = fs::write(runs_dir.join(&run_filename), &json);
        let _ = fs::write(runs_dir.join("last.json"), &json);
    }
}

pub fn run_judge(
    sources: &[PathBuf],
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let config_path = problem_dir.join("problem.toml");
    let config_content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "ConfigError",
                        "message": format!("Cannot read problem.toml at '{}': {e}", config_path.display()),
                        "hint": "Ensure the problem directory exists and contains problem.toml"
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!(
                    "Error: Cannot read problem.toml at '{}': {e}",
                    config_path.display()
                );
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let problem_config = match ProblemConfig::from_toml_str(&config_content, Some(&config_path)) {
        Ok(c) => c,
        Err(e) => return handle_config_error(e, format == OutputFormat::Json),
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &problem_config) {
        Ok(t) => t,
        Err(e) => {
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "IoError",
                        "message": format!("Failed to discover tests: {e}"),
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!("Error discovering tests: {e}");
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    if tests.is_empty() {
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "NoTestsFound",
                    "message": format!("No test cases (.in files) found in '{}'", problem_dir.join(&problem_config.tests.dir).display()),
                    "hint": "Check problem.toml [tests.dir] configuration"
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!(
                "Warning: No test cases found in '{}'.",
                problem_dir.join(&problem_config.tests.dir).display()
            );
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = match cli.sandbox {
        SandboxMode::Auto => "auto",
        SandboxMode::Full => "full",
        SandboxMode::Degraded => "degraded",
        SandboxMode::None => "none",
    };

    let judge_options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: cli.fail_fast,
        precise: cli.precise,
    };

    let scheduler = Scheduler::new(judge_options);
    let mut overall_exit = ExitCode::from(EXIT_SUCCESS);

    for source in sources {
        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        // Set up NDJSON event streaming if requested
        let (event_sender, event_thread) = if format == OutputFormat::Ndjson {
            let (tx, rx) = unbounded::<JudgeEvent>();
            let handle = thread::spawn(move || {
                for ev in rx {
                    if let Ok(line) = serde_json::to_string(&ev) {
                        println!("{line}");
                    }
                }
            });
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        if let Some(ref tx) = event_sender {
            let _ = tx.send(JudgeEvent::CompileStart {
                target: source.to_string_lossy().to_string(),
                compiler: "auto".to_string(),
            });
        }

        // 1. Compile source
        let compiled = match Compiler::compile(source, cli.no_cache) {
            Ok(res) => {
                if let Some(ref tx) = event_sender {
                    let _ = tx.send(JudgeEvent::CompileDone {
                        target: source.to_string_lossy().to_string(),
                        success: true,
                        duration_ms: res.duration_ms,
                        error_output: None,
                    });
                }
                res
            }
            Err(e) => {
                if let Some(ref tx) = event_sender {
                    let _ = tx.send(JudgeEvent::CompileDone {
                        target: source.to_string_lossy().to_string(),
                        success: false,
                        duration_ms: 0,
                        error_output: Some(e.to_string()),
                    });
                }

                if format == OutputFormat::Json {
                    let err_msg = match &e {
                        CompileError::Failed(msg) => msg.clone(),
                        other => other.to_string(),
                    };
                    let ce_report = serde_json::json!({
                        "schema_version": 1,
                        "problem": problem_config.name,
                        "submission": source.file_name().unwrap_or_default().to_string_lossy(),
                        "verdict": "CE",
                        "score": 0.0,
                        "max_time_ms": 0,
                        "max_memory_kb": 0,
                        "subtasks": [],
                        "tests": [],
                        "compile_error": err_msg
                    });
                    println!("{}", serde_json::to_string_pretty(&ce_report).unwrap());
                } else if format == OutputFormat::Human {
                    eprintln!("=== Compilation Error: {} ===", source.display());
                    eprintln!("{e}");
                }

                return ExitCode::from(EXIT_COMPILE_ERROR);
            }
        };

        // 2. Judge tests
        let judge_res = scheduler.judge_submission(
            problem_dir,
            &problem_config,
            &compiled,
            tests.clone(),
            cli.jobs,
            event_sender,
            cancel,
        );

        if let Some(h) = event_thread {
            let _ = h.join();
        }

        if cancel.is_cancelled() {
            return ExitCode::from(EXIT_INTERRUPTED);
        }

        match judge_res {
            Ok(report) => {
                save_run_report(problem_dir, &report);

                match format {
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    }
                    OutputFormat::Human => {
                        println!("{}", format_human_report(&report));
                    }
                    OutputFormat::Ndjson => {
                        // NDJSON events were already streamed to stdout
                    }
                }

                let code = match report.verdict {
                    Verdict::Accepted => EXIT_SUCCESS,
                    Verdict::JudgeFailed => EXIT_JUDGE_FAILED,
                    Verdict::CompileError => EXIT_COMPILE_ERROR,
                    _ => EXIT_NON_AC,
                };

                if code != EXIT_SUCCESS {
                    overall_exit = ExitCode::from(code);
                }
            }
            Err(e) => {
                if format == OutputFormat::Json {
                    let err_obj = serde_json::json!({
                        "ok": false,
                        "error": {
                            "kind": "JudgeFailed",
                            "message": e.to_string(),
                        }
                    });
                    println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
                } else {
                    eprintln!("Judging failed: {e}");
                }
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        }
    }

    overall_exit
}

pub fn run_last(problem_dir: &Path, format: OutputFormat) -> ExitCode {
    let last_path = problem_dir.join(".uj").join("runs").join("last.json");
    if !last_path.exists() {
        eprintln!(
            "No previous run found at '{}'. Run 'uj judge' first.",
            last_path.display()
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let content = match fs::read_to_string(&last_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read last run: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let report: SubmissionJudgeReport = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to parse last run: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    match format {
        OutputFormat::Json | OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        OutputFormat::Human => {
            println!("{}", format_human_report(&report));
        }
    }

    if report.verdict == Verdict::Accepted {
        ExitCode::from(EXIT_SUCCESS)
    } else {
        ExitCode::from(EXIT_NON_AC)
    }
}

pub fn run_show(
    problem_dir: &Path,
    run_id: Option<&str>,
    test_id: Option<&str>,
    format: OutputFormat,
) -> ExitCode {
    let run_file = if let Some(id) = run_id {
        let name = if id.ends_with(".json") {
            id.to_string()
        } else {
            format!("{id}.json")
        };
        problem_dir.join(".uj").join("runs").join(name)
    } else {
        problem_dir.join(".uj").join("runs").join("last.json")
    };

    if !run_file.exists() {
        eprintln!("Run file '{}' does not exist.", run_file.display());
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let content = match fs::read_to_string(&run_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read run file: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let report: SubmissionJudgeReport = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to parse run report: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    if let Some(target_test) = test_id {
        if let Some(test_res) = report.tests.iter().find(|t| t.id == target_test) {
            match format {
                OutputFormat::Json | OutputFormat::Ndjson => {
                    println!("{}", serde_json::to_string_pretty(test_res).unwrap());
                }
                OutputFormat::Human => {
                    println!("{}", format_test_details(test_res));
                }
            }
            if test_res.verdict == Verdict::Accepted {
                ExitCode::from(EXIT_SUCCESS)
            } else {
                ExitCode::from(EXIT_NON_AC)
            }
        } else {
            eprintln!(
                "Test case '{target_test}' not found in run '{}'.",
                run_file.display()
            );
            ExitCode::from(EXIT_USAGE_OR_CONFIG)
        }
    } else {
        match format {
            OutputFormat::Json | OutputFormat::Ndjson => {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            }
            OutputFormat::Human => {
                println!("{}", format_human_report(&report));
            }
        }
        if report.verdict == Verdict::Accepted {
            ExitCode::from(EXIT_SUCCESS)
        } else {
            ExitCode::from(EXIT_NON_AC)
        }
    }
}

pub fn run_clean(problem_dir: &Path) -> ExitCode {
    let uj_dir = problem_dir.join(".uj");
    if uj_dir.exists() {
        let cache_dir = uj_dir.join("cache");
        let runs_dir = uj_dir.join("runs");

        let _ = fs::remove_dir_all(&cache_dir);
        let _ = fs::remove_dir_all(&runs_dir);

        println!("Cache and run records in '{}' cleaned.", uj_dir.display());
    } else {
        println!("No .uj directory found; nothing to clean.");
    }
    ExitCode::from(EXIT_SUCCESS)
}

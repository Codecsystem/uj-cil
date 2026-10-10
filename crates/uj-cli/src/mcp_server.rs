//! Model Context Protocol (MCP) server implementation over stdio.
//!
//! Exposes `uj` functionality (judge, build, verify, stress, pipeline, doctor, show, contest)
//! to AI assistants (Claude, Antigravity, Cursor, etc.) via JSON-RPC 2.0.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uj_core::config::{ProblemConfig, SolutionTag};
use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_engine::compiler::Compiler;
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_report::result::SubmissionJudgeReport;
use uj_sandbox::{CancelToken, create_sandbox};

use crate::doctor;
use crate::error::EXIT_SUCCESS;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

pub fn run_mcp_server() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    let cancel = CancelToken::new();
    let cancel_clone = cancel.clone();
    let _ = ctrlc::set_handler(move || {
        cancel_clone.cancel();
    });

    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if cancel.is_cancelled() {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                        data: None,
                    }),
                };
                let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
                let _ = writer.flush();
                line.clear();
                continue;
            }
        };

        if let Some(resp) = handle_rpc_request(req, &cancel) {
            let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
            let _ = writer.flush();
        }

        line.clear();
    }

    ExitCode::from(EXIT_SUCCESS)
}

fn handle_rpc_request(req: JsonRpcRequest, cancel: &CancelToken) -> Option<JsonRpcResponse> {
    let id = req.id.unwrap_or(Value::Null);

    // Notifications (no id)
    if id.is_null() && req.method.starts_with("notifications/") {
        return None;
    }

    match req.method.as_str() {
        "initialize" => {
            let res = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "uj",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(res),
                error: None,
            })
        }
        "ping" => Some(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(serde_json::json!({})),
            error: None,
        }),
        "tools/list" => {
            let tools = get_tools_list();
            Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(serde_json::json!({ "tools": tools })),
                error: None,
            })
        }
        "tools/call" => {
            let params = req.params.unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or_default();

            let tool_res = dispatch_tool_call(name, &arguments, cancel);
            match tool_res {
                Ok(content_val) => {
                    let res = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": content_val
                            }
                        ],
                        "isError": false
                    });
                    Some(JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: Some(res),
                        error: None,
                    })
                }
                Err(e) => {
                    let res = serde_json::json!({
                        "content": [
                            {
                                "type": "text",
                                "text": format!("Error: {e}")
                            }
                        ],
                        "isError": true
                    });
                    Some(JsonRpcResponse {
                        jsonrpc: "2.0",
                        id,
                        result: Some(res),
                        error: None,
                    })
                }
            }
        }
        _ => Some(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("Method '{}' not found", req.method),
                data: None,
            }),
        }),
    }
}

fn get_tools_list() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "judge",
            "description": "Judge one or more submission source files against problem test cases",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Path to problem directory containing problem.toml (default: '.')"
                    },
                    "submission": {
                        "type": "string",
                        "description": "Path to submission source file (e.g. 'solutions/sol.cpp')"
                    },
                    "sources": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of submission source paths (if multiple)"
                    }
                },
                "required": ["submission"]
            }
        }),
        serde_json::json!({
            "name": "build",
            "description": "Compile problem binaries (checker, validator, solutions, generators)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Problem directory containing problem.toml (default: '.')"
                    },
                    "targets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Specific compilation targets or empty for all"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "verify",
            "description": "Verify all [[solutions]] in problem.toml against test data according to their tags",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Problem directory containing problem.toml (default: '.')"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "pipeline",
            "description": "Execute full problem preparation pipeline (build -> gen -> validate -> ans -> verify -> tl)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Problem directory containing problem.toml (default: '.')"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "stress",
            "description": "Run differential testing (stress testing) with generator, target solution and brute force oracle",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Problem directory (default: '.')"
                    },
                    "generator": {
                        "type": "string",
                        "description": "Generator executable or source"
                    },
                    "solution": {
                        "type": "string",
                        "description": "Target solution to test"
                    },
                    "brute": {
                        "type": "string",
                        "description": "Brute force oracle solution"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Maximum test iterations (default: 100)"
                    }
                },
                "required": ["generator", "solution", "brute"]
            }
        }),
        serde_json::json!({
            "name": "doctor",
            "description": "Run system environment, sandbox isolation, and problem configuration diagnostics",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "boolean",
                        "description": "Whether to check problem test data and configuration health"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "show",
            "description": "Show detailed inspection of a judging run or specific test case",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "problem": {
                        "type": "string",
                        "description": "Problem directory (default: '.')"
                    },
                    "run_id": {
                        "type": "string",
                        "description": "Run ID or empty for last run"
                    },
                    "test": {
                        "type": "string",
                        "description": "Specific test ID to inspect"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "contest",
            "description": "Judge entire contest directory with multiple problems and contestants",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": {
                        "type": "string",
                        "description": "Contest root directory"
                    }
                },
                "required": ["dir"]
            }
        }),
    ]
}

fn dispatch_tool_call(
    name: &str,
    args: &Value,
    cancel: &CancelToken,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match name {
        "doctor" => {
            let problem_mode = args
                .get("problem")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let report = doctor::run_doctor(problem_mode, false);
            Ok(serde_json::to_string_pretty(&report)?)
        }
        "build" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let config_path = problem_dir.join("problem.toml");
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;

            let mut targets = Vec::new();
            if let Some(arr) = args.get("targets").and_then(|v| v.as_array()) {
                for t in arr {
                    if let Some(s) = t.as_str() {
                        targets.push(problem_dir.join(s));
                    }
                }
            } else {
                if let Some(ref v) = cfg.validator {
                    targets.push(problem_dir.join(v));
                }
                if let Some(ref chk) = cfg.checker.path {
                    targets.push(problem_dir.join(chk));
                }
                if let Some(ref int) = cfg.interactor {
                    targets.push(problem_dir.join(int));
                }
                for g in &cfg.tests.generators {
                    targets.push(problem_dir.join(&g.generator));
                }
                for sol in &cfg.solutions {
                    targets.push(problem_dir.join(&sol.path));
                }
            }

            let mut results = Vec::new();
            for target in targets {
                if target.extension().is_some_and(|e| e == "exe") {
                    continue;
                }
                let c = Compiler::compile(&target, false)?;
                results.push(serde_json::json!({
                    "target": target.to_string_lossy(),
                    "duration_ms": c.duration_ms,
                    "cached": c.cached
                }));
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "compiled": results
            }))?)
        }
        "judge" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let config_path = problem_dir.join("problem.toml");
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;
            let tests = uj_core::tests::discover_tests(problem_dir, &cfg)?;

            let mut sources = Vec::new();
            if let Some(sub) = args.get("submission").and_then(|v| v.as_str()) {
                sources.push(PathBuf::from(sub));
            } else if let Some(arr) = args.get("sources").and_then(|v| v.as_array()) {
                for s in arr {
                    if let Some(str_val) = s.as_str() {
                        sources.push(PathBuf::from(str_val));
                    }
                }
            }

            if sources.is_empty() {
                return Err("No submission source provided".into());
            }

            let judge_options = JudgeOptions {
                sandbox_mode: "auto".to_string(),
                require_tier: false,
                fail_fast: false,
                precise: true,
            };
            let scheduler = Scheduler::new(judge_options);
            let mut reports = Vec::new();

            for src in &sources {
                let compiled = Compiler::compile(src, false)?;
                let sub_name = src
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let report = scheduler.judge_submission(
                    problem_dir,
                    &cfg,
                    &compiled,
                    &sub_name,
                    tests.clone(),
                    None,
                    None,
                    cancel,
                )?;
                reports.push(report);
            }

            if reports.len() == 1 {
                Ok(serde_json::to_string_pretty(&reports[0])?)
            } else {
                Ok(serde_json::to_string_pretty(&reports)?)
            }
        }
        "verify" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let config_path = problem_dir.join("problem.toml");
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;
            let tests = uj_core::tests::discover_tests(problem_dir, &cfg)?;

            let judge_options = JudgeOptions {
                sandbox_mode: "auto".to_string(),
                require_tier: false,
                fail_fast: false,
                precise: true,
            };
            let scheduler = Scheduler::new(judge_options);

            let mut verifications = Vec::new();
            let mut all_match = true;

            for sol in &cfg.solutions {
                let sol_path = problem_dir.join(&sol.path);
                let compiled = Compiler::compile(&sol_path, false)?;
                let sub_name = sol.path.to_string_lossy().to_string();
                let report = scheduler.judge_submission(
                    problem_dir,
                    &cfg,
                    &compiled,
                    &sub_name,
                    tests.clone(),
                    None,
                    None,
                    cancel,
                )?;

                let (matched, reason) = check_solution_match(&sol.tag, &report);
                if !matched {
                    all_match = false;
                }

                verifications.push(serde_json::json!({
                    "solution": sol.path.to_string_lossy(),
                    "tag": format!("{:?}", sol.tag),
                    "actual_verdict": report.verdict,
                    "score": report.score,
                    "matched": matched,
                    "reason": reason
                }));
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": all_match,
                "solutions": verifications
            }))?)
        }
        "pipeline" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let config_path = problem_dir.join("problem.toml");
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;

            // 1. Build
            let mut compiled_count = 0;
            if let Some(ref val) = cfg.validator {
                Compiler::compile(&problem_dir.join(val), false)?;
                compiled_count += 1;
            }
            if let Some(ref chk) = cfg.checker.path {
                Compiler::compile(&problem_dir.join(chk), false)?;
                compiled_count += 1;
            }
            for sol in &cfg.solutions {
                Compiler::compile(&problem_dir.join(&sol.path), false)?;
                compiled_count += 1;
            }

            // 2. Discover tests
            let tests = uj_core::tests::discover_tests(problem_dir, &cfg)?;

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "step": "pipeline_complete",
                "components_compiled": compiled_count,
                "tests_count": tests.len()
            }))?)
        }
        "stress" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let gen_str = args
                .get("generator")
                .and_then(|v| v.as_str())
                .ok_or("generator required")?;
            let sol_str = args
                .get("solution")
                .and_then(|v| v.as_str())
                .ok_or("solution required")?;
            let brute_str = args
                .get("brute")
                .and_then(|v| v.as_str())
                .ok_or("brute required")?;
            let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(100) as usize;

            let compiled_gen = Compiler::compile(&problem_dir.join(gen_str), false)?;
            let compiled_sol = Compiler::compile(&problem_dir.join(sol_str), false)?;
            let compiled_brute = Compiler::compile(&problem_dir.join(brute_str), false)?;

            let sandbox = create_sandbox("auto", false)?;
            let mut tested = 0;

            for seed in 1..=count {
                if cancel.is_cancelled() {
                    return Err("Cancelled".into());
                }

                // Run generator
                let gen_spec = RunSpec {
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
                    limits: Limits::new(5000, 512 * 1024 * 1024),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: true,
                };
                let gen_res = sandbox.run(&gen_spec, cancel)?;
                let in_bytes = gen_res.stdout.map(|o| o.bytes).unwrap_or_default();
                let tmp_dir = problem_dir.join(".uj").join("tmp");
                let _ = fs::create_dir_all(&tmp_dir);
                let tmp_in_path = tmp_dir.join(format!("stress_{seed}.in"));
                let _ = fs::write(&tmp_in_path, &in_bytes);

                // Run sol
                let sol_spec = RunSpec {
                    argv: vec![compiled_sol.binary_path.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: problem_dir.to_path_buf(),
                    ro_binds: vec![],
                    stdin: Stdio::File(tmp_in_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: Limits::new(2000, 256 * 1024 * 1024),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };
                let sol_res = sandbox.run(&sol_spec, cancel)?;
                let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();

                // Run brute
                let brute_spec = RunSpec {
                    argv: vec![compiled_brute.binary_path.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: problem_dir.to_path_buf(),
                    ro_binds: vec![],
                    stdin: Stdio::File(tmp_in_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: Limits::new(5000, 256 * 1024 * 1024),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };
                let brute_res = sandbox.run(&brute_spec, cancel)?;
                let brute_bytes = brute_res.stdout.map(|o| o.bytes).unwrap_or_default();
                let _ = fs::remove_file(&tmp_in_path);

                tested += 1;

                // Compare with wcmp
                let cmp_res =
                    uj_checker::builtin::BuiltinCheckerType::Wcmp.check(&sol_bytes, &brute_bytes);

                if cmp_res.verdict != Verdict::Accepted {
                    return Ok(serde_json::to_string_pretty(&serde_json::json!({
                        "ok": false,
                        "mismatch": {
                            "seed": seed,
                            "input": String::from_utf8_lossy(&in_bytes),
                            "solution_output": String::from_utf8_lossy(&sol_bytes),
                            "brute_output": String::from_utf8_lossy(&brute_bytes),
                            "message": cmp_res.message
                        },
                        "iterations": tested
                    }))?);
                }
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "tested": tested,
                "message": "All stress tests passed"
            }))?)
        }
        "show" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let run_file = if let Some(id) = args.get("run_id").and_then(|v| v.as_str()) {
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
                return Err(format!("Run file '{}' does not exist", run_file.display()).into());
            }

            let content = fs::read_to_string(&run_file)?;
            let report: SubmissionJudgeReport = serde_json::from_str(&content)?;

            if let Some(target_test) = args.get("test").and_then(|v| v.as_str()) {
                if let Some(test_res) = report.tests.iter().find(|t| t.id == target_test) {
                    Ok(serde_json::to_string_pretty(test_res)?)
                } else {
                    Err(format!("Test '{target_test}' not found in run").into())
                }
            } else {
                Ok(serde_json::to_string_pretty(&report)?)
            }
        }
        "contest" => {
            let dir_str = args
                .get("dir")
                .and_then(|v| v.as_str())
                .ok_or("dir required")?;
            let contest_dir = Path::new(dir_str);
            if !contest_dir.exists() {
                return Err(format!("Contest directory '{}' not found", dir_str).into());
            }

            // We can invoke contest_cmd discovery and judging directly
            let report_path = contest_dir.join("contest_report.csv");
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "contest_dir": dir_str,
                "csv_report": report_path.to_string_lossy()
            }))?)
        }
        _ => Err(format!("Unknown tool '{name}'").into()),
    }
}

fn check_solution_match(tag: &SolutionTag, report: &SubmissionJudgeReport) -> (bool, String) {
    match tag {
        SolutionTag::Main | SolutionTag::Accepted => {
            if report.verdict == Verdict::Accepted {
                (true, "Accepted as expected".to_string())
            } else {
                (
                    false,
                    format!("Expected Accepted, got {:?}", report.verdict),
                )
            }
        }
        SolutionTag::WrongAnswer => {
            if report.verdict == Verdict::WrongAnswer {
                (true, "WrongAnswer as expected".to_string())
            } else {
                (
                    false,
                    format!("Expected WrongAnswer, got {:?}", report.verdict),
                )
            }
        }
        SolutionTag::TimeLimit => {
            if report.verdict == Verdict::TimeLimitExceeded {
                (true, "TLE as expected".to_string())
            } else {
                (false, format!("Expected TLE, got {:?}", report.verdict))
            }
        }
        SolutionTag::MemoryLimit => {
            if report.verdict == Verdict::MemoryLimitExceeded {
                (true, "MLE as expected".to_string())
            } else {
                (false, format!("Expected MLE, got {:?}", report.verdict))
            }
        }
        SolutionTag::RuntimeError => {
            if report.verdict == Verdict::RuntimeError {
                (true, "RE as expected".to_string())
            } else {
                (false, format!("Expected RE, got {:?}", report.verdict))
            }
        }
        SolutionTag::Rejected => {
            if report.verdict != Verdict::Accepted {
                (true, format!("Rejected as expected ({:?})", report.verdict))
            } else {
                (false, "Expected rejection, but got Accepted".to_string())
            }
        }
        SolutionTag::Partial => {
            if report.score > 0.0 && report.score < 100.0 {
                (
                    true,
                    format!("Partial score {:.1} as expected", report.score),
                )
            } else {
                (
                    false,
                    format!("Expected partial score (0, 100), got {:.1}", report.score),
                )
            }
        }
    }
}

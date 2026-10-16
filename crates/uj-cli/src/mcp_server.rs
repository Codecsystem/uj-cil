//! Model Context Protocol (MCP) server implementation over stdio.
//!
//! Exposes `uj` functionality (judge, build, verify, stress, pipeline, doctor, show, contest)
//! to AI assistants (Claude, Antigravity, Cursor, etc.) via JSON-RPC 2.0.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uj_core::config::ProblemConfig;
use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_engine::compiler::CompileSession;
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_report::result::SubmissionJudgeReport;
use uj_sandbox::{CancelToken, create_sandbox};

use crate::cli::{Cli, OutputFormat};
use crate::doctor;
use crate::error::EXIT_SUCCESS;

#[derive(Debug, Deserialize, Clone)]
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
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    let cancel = CancelToken::new();
    let cancel_clone = cancel.clone();
    let _ = ctrlc::set_handler(move || {
        cancel_clone.cancel();
    });

    let (line_tx, line_rx) = crossbeam_channel::unbounded::<String>();
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line_tx.send(line.clone()).is_err() {
                break;
            }
            line.clear();
        }
    });

    let active_task_cancel: Arc<Mutex<Option<CancelToken>>> = Arc::new(Mutex::new(None));

    while let Ok(line) = line_rx.recv() {
        if cancel.is_cancelled() {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
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
                continue;
            }
        };

        // Validate JSON-RPC 2.0 version
        if req.jsonrpc != "2.0" {
            if req.id.is_some() {
                let err_resp = JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: req.id.unwrap_or(Value::Null),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32600,
                        message: "Invalid Request: jsonrpc must be '2.0'".to_string(),
                        data: None,
                    }),
                };
                let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
                let _ = writer.flush();
            }
            continue;
        }

        // Handle cancellation notifications
        let is_cancel = req.id.is_none()
            && (req.method == "notifications/cancelled"
                || req.method == "$/cancelRequest"
                || req.method == "cancelled");
        if is_cancel {
            if let Some(token) = active_task_cancel.lock().unwrap().as_ref() {
                token.cancel();
            }
            continue;
        }

        // Ignore standard notifications (no response per JSON-RPC spec)
        if req.id.is_none()
            && (req.method.starts_with("notifications/") || req.method.starts_with("$/"))
        {
            continue;
        }

        if req.method == "tools/call" {
            let task_cancel = CancelToken::new();
            *active_task_cancel.lock().unwrap() = Some(task_cancel.clone());
            let (resp_tx, resp_rx) = crossbeam_channel::bounded::<JsonRpcResponse>(1);
            let cancel_for_worker = task_cancel.clone();
            let req_clone = req;

            thread::spawn(move || {
                let resp = handle_rpc_request(req_clone, &cancel_for_worker);
                if let Some(r) = resp {
                    let _ = resp_tx.send(r);
                }
            });

            loop {
                crossbeam_channel::select! {
                    recv(resp_rx) -> resp_res => {
                        if let Ok(resp) = resp_res {
                            let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                            let _ = writer.flush();
                        }
                        *active_task_cancel.lock().unwrap() = None;
                        break;
                    }
                    recv(line_rx) -> incoming_res => {
                        match incoming_res {
                            Ok(inc_line) => {
                                let inc_trimmed = inc_line.trim();
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(inc_trimmed) {
                                    let method = val.get("method").and_then(|m| m.as_str()).unwrap_or("");
                                    let cancel_notify = val.get("id").is_none()
                                        && (method == "notifications/cancelled"
                                            || method == "$/cancelRequest"
                                            || method == "cancelled");
                                    if cancel_notify {
                                        task_cancel.cancel();
                                    }
                                }
                            }
                            Err(_) => {
                                task_cancel.cancel();
                                *active_task_cancel.lock().unwrap() = None;
                                break;
                            }
                        }
                    }
                }
            }
        } else if let Some(resp) = handle_rpc_request(req, &cancel) {
            let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
            let _ = writer.flush();
        }
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
                    },
                    "seed": {
                        "type": "integer",
                        "description": "Starting random seed (default: 1)"
                    },
                    "gen_args": {
                        "type": "string",
                        "description": "Generator argument template; placeholders {seed} and {iter} are expanded (default: '{seed}')"
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
                    },
                    "problems": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of problem names to filter"
                    },
                    "contestants": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of contestant names to filter"
                    }
                },
                "required": ["dir"]
            }
        }),
        serde_json::json!({
            "name": "init_problem",
            "description": "Initialize a new problem directory structure with problem.toml, validator, checker, generator and solutions",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": {
                        "type": "string",
                        "description": "Target problem directory"
                    },
                    "name": {
                        "type": "string",
                        "description": "Problem identifier name"
                    },
                    "title": {
                        "type": "string",
                        "description": "Human-readable problem title"
                    },
                    "type": {
                        "type": "string",
                        "description": "Problem type: traditional, interactive, etc."
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

            let mut targets: Vec<(PathBuf, bool)> = Vec::new();
            if let Some(arr) = args.get("targets").and_then(|v| v.as_array()) {
                for t in arr {
                    if let Some(s) = t.as_str() {
                        let p = problem_dir.join(s);
                        let is_sol = cfg.solutions.iter().any(|sol| {
                            problem_dir.join(&sol.path) == p || sol.path == Path::new(s)
                        });
                        targets.push((p, is_sol));
                    }
                }
            } else {
                if let Some(ref v) = cfg.validator {
                    targets.push((problem_dir.join(v), false));
                }
                if let Some(ref chk) = cfg.checker.path {
                    targets.push((problem_dir.join(chk), false));
                }
                if let Some(ref int) = cfg.interactor {
                    targets.push((problem_dir.join(int), false));
                }
                for g in &cfg.tests.generators {
                    targets.push((problem_dir.join(&g.generator), false));
                }
                for sol in &cfg.solutions {
                    targets.push((problem_dir.join(&sol.path), true));
                }
            }

            let session = CompileSession::new();
            let mut results = Vec::new();
            for (target, is_sol) in targets {
                if target.extension().is_some_and(|e| e == "exe") {
                    continue;
                }
                let c = if is_sol {
                    session.compile_problem_solution(problem_dir, &cfg, &target, false)?
                } else {
                    session.compile(&target, false)?
                };
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
            let session = CompileSession::new();
            let scheduler = Scheduler::with_session(judge_options, session.clone());
            let mut reports = Vec::new();

            for src in &sources {
                let compiled = session.compile_problem_solution(problem_dir, &cfg, src, false)?;
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
                reports.push(report.clone());
                let _ = crate::judge_cmd::save_run_report(problem_dir, &report);
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
            let session = CompileSession::new();
            let scheduler = Scheduler::with_session(judge_options, session.clone());

            let mut verifications = Vec::new();
            let mut all_match = true;

            for sol in &cfg.solutions {
                let sol_path = problem_dir.join(&sol.path);
                let compiled =
                    session.compile_problem_solution(problem_dir, &cfg, &sol_path, false)?;
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

                let failures = crate::author_cmd::check_solution_tag(sol, &report, &cfg);
                let matched = failures.is_empty();
                let reason = if matched {
                    format!("{:?} as expected (score: {:.1})", sol.tag, report.score)
                } else {
                    all_match = false;
                    failures.join("; ")
                };

                let max_time_test = report
                    .tests
                    .iter()
                    .max_by_key(|t| t.time_ms)
                    .map(|t| t.id.clone());
                let first_fail = report
                    .tests
                    .iter()
                    .find(|t| t.verdict != Verdict::Accepted && t.verdict != Verdict::Skipped);

                verifications.push(serde_json::json!({
                    "solution": sol.path.to_string_lossy(),
                    "path": sol.path.to_string_lossy().replace('\\', "/"),
                    "tag": sol.tag.as_str(),
                    "actual_verdict": report.verdict,
                    "verdict": report.verdict,
                    "score": report.score,
                    "max_time_ms": report.max_time_ms,
                    "max_time_test": max_time_test,
                    "max_memory_kb": report.max_memory_kb,
                    "first_fail_test": first_fail.map(|t| t.id.clone()),
                    "first_fail_verdict": first_fail.map(|t| t.verdict),
                    "matched": matched,
                    "matches_tag": matched,
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
            if !config_path.exists() {
                return Err(
                    format!("problem.toml not found in '{}'", problem_dir.display()).into(),
                );
            }
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;

            // Fail-closed if configured generator source does not exist
            for g in &cfg.tests.generators {
                let gen_path = problem_dir.join(&g.generator);
                if !gen_path.exists() {
                    return Err(format!(
                        "Generator source '{}' does not exist",
                        gen_path.display()
                    )
                    .into());
                }
            }

            let cli = Cli::parse_from(["uj", "pipeline"]);
            let code =
                crate::author_cmd::run_pipeline(problem_dir, &cli, OutputFormat::Silent, cancel);
            if code != ExitCode::from(EXIT_SUCCESS) {
                return Err(
                    format!("Pipeline execution failed for '{}'", problem_dir.display()).into(),
                );
            }

            let tests = uj_core::tests::discover_tests(problem_dir, &cfg)?;
            if tests.is_empty() {
                return Err(format!(
                    "Pipeline completed but no test cases found in '{}'",
                    problem_dir.display()
                )
                .into());
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "step": "pipeline_complete",
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
            let seed_start = args.get("seed").and_then(|v| v.as_u64()).unwrap_or(1);
            let gen_args_tmpl = args
                .get("gen_args")
                .and_then(|v| v.as_str())
                .unwrap_or("{seed}");

            let config_path = problem_dir.join("problem.toml");
            let cfg_opt = if config_path.exists() {
                let content = fs::read_to_string(&config_path)?;
                Some(ProblemConfig::from_toml_str(&content, Some(&config_path))?)
            } else {
                None
            };

            let session = CompileSession::new();
            let compiled_gen = session.compile(&problem_dir.join(gen_str), false)?;
            let compiled_sol = if let Some(ref cfg) = cfg_opt {
                session.compile_problem_solution(
                    problem_dir,
                    cfg,
                    &problem_dir.join(sol_str),
                    false,
                )?
            } else {
                session.compile(&problem_dir.join(sol_str), false)?
            };
            let compiled_brute = if let Some(ref cfg) = cfg_opt {
                session.compile_problem_solution(
                    problem_dir,
                    cfg,
                    &problem_dir.join(brute_str),
                    false,
                )?
            } else {
                session.compile(&problem_dir.join(brute_str), false)?
            };
            let (resolved_checker, pe_as_wa, sol_limits) = if let Some(ref cfg) = cfg_opt {
                let lim = cfg
                    .to_limits()
                    .unwrap_or_else(|_| Limits::new(2000, 256 * 1024 * 1024));
                let pe = cfg.checker.pe_as_wa;
                let chk = match uj_engine::context::ProblemContext::prepare(
                    0,
                    problem_dir,
                    cfg,
                    &session,
                ) {
                    Ok(p_ctx) => p_ctx.resolved_checker.clone(),
                    Err(_) => {
                        let name = cfg.checker.name.as_deref().unwrap_or("wcmp");
                        uj_engine::judge::ResolvedChecker::Builtin(
                            uj_checker::builtin::BuiltinCheckerType::parse(name)
                                .unwrap_or(uj_checker::builtin::BuiltinCheckerType::Wcmp),
                        )
                    }
                };
                (chk, pe, lim)
            } else {
                (
                    uj_engine::judge::ResolvedChecker::Builtin(
                        uj_checker::builtin::BuiltinCheckerType::Wcmp,
                    ),
                    false,
                    Limits::new(2000, 256 * 1024 * 1024),
                )
            };
            let mut brute_limits = sol_limits.clone();
            brute_limits.cpu_time_ms = brute_limits.cpu_time_ms.saturating_mul(3).max(5000);
            brute_limits.wall_time_ms = brute_limits.cpu_time_ms.saturating_mul(3).max(15000);

            let sandbox = create_sandbox("auto", false)?;
            let slot_box = uj_sandbox::create_slot_box_dir(problem_dir, 6000)?;
            let gen_bin = uj_sandbox::get_or_prepare_authorized_binary(
                &compiled_gen.binary_path,
                problem_dir,
            )
            .unwrap_or_else(|_| compiled_gen.binary_path.clone());
            let sol_bin = uj_sandbox::get_or_prepare_authorized_binary(
                &compiled_sol.binary_path,
                problem_dir,
            )
            .unwrap_or_else(|_| compiled_sol.binary_path.clone());
            let brute_bin = uj_sandbox::get_or_prepare_authorized_binary(
                &compiled_brute.binary_path,
                problem_dir,
            )
            .unwrap_or_else(|_| compiled_brute.binary_path.clone());
            let _ = sandbox.authorize_executable(&gen_bin);
            let _ = sandbox.authorize_executable(&sol_bin);
            let _ = sandbox.authorize_executable(&brute_bin);
            match &resolved_checker {
                uj_engine::judge::ResolvedChecker::Testlib(chk_exe)
                | uj_engine::judge::ResolvedChecker::Lemon { path: chk_exe, .. } => {
                    let _ = sandbox.authorize_executable(chk_exe);
                }
                uj_engine::judge::ResolvedChecker::Builtin(_) => {}
            }

            let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
            let mut tested = 0;

            for iter_idx in 0..count {
                if cancel.is_cancelled() {
                    return Err("Cancelled".into());
                }

                let iter_num = iter_idx + 1;
                let seed = seed_start + iter_idx as u64;
                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);

                let rendered_args = gen_args_tmpl
                    .replace("{seed}", &seed.to_string())
                    .replace("{iter}", &iter_num.to_string());
                let mut gen_argv = vec![gen_bin.as_os_str().to_os_string()];
                for arg in rendered_args.split_whitespace() {
                    gen_argv.push(std::ffi::OsString::from(arg));
                }

                // Run generator in isolated slot box
                let gen_spec = RunSpec {
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
                    limits: Limits::new(5000, 512 * 1024 * 1024),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: true,
                };
                let gen_res = sandbox.run(&gen_spec, cancel)?;
                if !gen_res.status.is_success() || gen_res.killed_by.is_some() || gen_res.oom_killed
                {
                    let stderr_msg = String::from_utf8_lossy(&gen_res.stderr_tail);
                    return Err(format!(
                        "Generator failed on seed {seed}: status={:?}, killed_by={:?}, oom={}. Stderr: {}",
                        gen_res.status,
                        gen_res.killed_by,
                        gen_res.oom_killed,
                        stderr_msg.trim()
                    )
                    .into());
                }
                let in_bytes = gen_res.stdout.map(|o| o.bytes).unwrap_or_default();
                if in_bytes.is_empty() {
                    return Err(format!(
                        "Generator produced empty stdout on seed {seed} (hint: check if gen.cpp ignores seeds outside fixed test IDs)"
                    )
                    .into());
                }
                let tmp_in_path = scratch_dir.join(format!("mcp_stress_{seed}.in"));
                let _ = fs::write(&tmp_in_path, &in_bytes);

                // Run sol in isolated slot box
                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                let sol_spec = RunSpec {
                    argv: vec![sol_bin.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: slot_box.box_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(tmp_in_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: sol_limits.clone(),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };
                let sol_res = sandbox.run(&sol_spec, cancel)?;
                let sol_bytes = sol_res.stdout.map(|o| o.bytes).unwrap_or_default();

                // Run brute in isolated slot box
                let _ = uj_sandbox::clean_box_dir_contents(&slot_box.box_dir);
                let brute_spec = RunSpec {
                    argv: vec![brute_bin.as_os_str().to_os_string()],
                    env: vec![],
                    work_dir: slot_box.box_dir.clone(),
                    ro_binds: vec![],
                    stdin: Stdio::File(tmp_in_path.clone()),
                    stdout: Stdio::Pipe,
                    stderr: Stdio::Pipe,
                    extra_fds: vec![],
                    limits: brute_limits.clone(),
                    cpu_affinity: None,
                    seccomp_profile: Default::default(),
                    trusted: false,
                };
                let brute_res = sandbox.run(&brute_spec, cancel)?;
                let brute_bytes = brute_res.stdout.map(|o| o.bytes).unwrap_or_default();

                tested += 1;

                let cmp_res = match &resolved_checker {
                    uj_engine::judge::ResolvedChecker::Builtin(b) => {
                        let mut cr = b.check(&sol_bytes, &brute_bytes);
                        if pe_as_wa && cr.verdict == Verdict::PresentationError {
                            cr.verdict = Verdict::WrongAnswer;
                        }
                        cr
                    }
                    uj_engine::judge::ResolvedChecker::Testlib(chk_exe) => {
                        let out_tmp = scratch_dir.join(format!("mcp_stress_{seed}.out"));
                        let ans_tmp = scratch_dir.join(format!("mcp_stress_{seed}.ans"));
                        let _ = fs::write(&out_tmp, &sol_bytes);
                        let _ = fs::write(&ans_tmp, &brute_bytes);
                        let cr = uj_checker::testlib::run_testlib_checker(
                            sandbox.as_ref(),
                            chk_exe,
                            &tmp_in_path,
                            &out_tmp,
                            &ans_tmp,
                            pe_as_wa,
                            cancel,
                        );
                        let _ = fs::remove_file(&out_tmp);
                        let _ = fs::remove_file(&ans_tmp);
                        cr
                    }
                    uj_engine::judge::ResolvedChecker::Lemon {
                        path: chk_exe,
                        full_score,
                    } => {
                        let out_tmp = scratch_dir.join(format!("mcp_stress_{seed}.out"));
                        let ans_tmp = scratch_dir.join(format!("mcp_stress_{seed}.ans"));
                        let _ = fs::write(&out_tmp, &sol_bytes);
                        let _ = fs::write(&ans_tmp, &brute_bytes);
                        let cr = uj_checker::lemon::run_lemon_checker(
                            sandbox.as_ref(),
                            chk_exe,
                            &tmp_in_path,
                            &out_tmp,
                            &ans_tmp,
                            *full_score,
                            cancel,
                        );
                        let _ = fs::remove_file(&out_tmp);
                        let _ = fs::remove_file(&ans_tmp);
                        cr
                    }
                };
                let _ = fs::remove_file(&tmp_in_path);

                if !cmp_res.verdict.is_accepted() {
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

            let problems: Vec<String> = args
                .get("problems")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let contestants: Vec<String> = args
                .get("contestants")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let cli = Cli::parse_from(["uj", "contest", dir_str]);
            let code = crate::contest_cmd::run_contest(
                contest_dir,
                &problems,
                &contestants,
                &cli,
                OutputFormat::Silent,
                cancel,
            );
            if code != ExitCode::from(EXIT_SUCCESS) {
                return Err(format!("Contest judging failed for '{dir_str}'").into());
            }

            let report_path = contest_dir.join("contest_report.csv");
            if !report_path.exists() {
                return Err(format!(
                    "Contest report '{}' was not generated",
                    report_path.display()
                )
                .into());
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "contest_dir": dir_str,
                "csv_report": report_path.to_string_lossy().to_string()
            }))?)
        }
        "init_problem" => {
            let dir_str = args
                .get("dir")
                .and_then(|v| v.as_str())
                .ok_or("dir required")?;
            let dir = Path::new(dir_str);
            let ptype = args
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("traditional");

            let code = crate::author_cmd::run_init(dir, ptype, OutputFormat::Silent);
            if code != ExitCode::from(EXIT_SUCCESS) {
                return Err(format!("Failed to initialize problem at '{dir_str}'").into());
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "dir": dir_str,
                "message": format!("Problem initialized at '{dir_str}'")
            }))?)
        }
        _ => Err(format!("Unknown tool '{name}'").into()),
    }
}

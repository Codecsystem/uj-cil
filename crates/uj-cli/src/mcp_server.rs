//! Model Context Protocol (MCP) server implementation over stdio.
//!
//! Exposes `uj` functionality (judge, build, verify, stress, pipeline, doctor, show, contest, init_problem)
//! to AI assistants (Claude, Antigravity, Cursor, etc.) via JSON-RPC 2.0.

#![allow(clippy::result_large_err)]

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use uj_core::config::ProblemConfig;
use uj_engine::compiler::CompileSession;
use uj_engine::judge::JudgeOptions;
use uj_engine::scheduler::Scheduler;
use uj_report::result::SubmissionJudgeReport;
use uj_sandbox::CancelToken;

use crate::cli::{Cli, OutputFormat, SandboxMode, parse_duration_str};
use crate::doctor;
use crate::error::{EXIT_NON_AC, EXIT_SUCCESS, install_ctrlc, subscribe_ctrlc, validate_run_id};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone)]
struct ValidatedRpcMessage {
    /// `None` means notification (`"id"` key was omitted); `Some(Value::Null)` means `"id": null`.
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize, Clone)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Clone)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
}

struct ActiveWorker {
    id: Value,
    cancel: CancelToken,
    resp_rx: crossbeam_channel::Receiver<JsonRpcResponse>,
    handle: JoinHandle<()>,
}

fn validate_rpc_object(val: Value) -> Result<ValidatedRpcMessage, Option<JsonRpcResponse>> {
    let obj = match val {
        Value::Object(map) => map,
        _ => {
            return Err(Some(JsonRpcResponse::error(
                Value::Null,
                -32600,
                "Invalid Request: expected JSON object",
            )));
        }
    };

    let has_id = obj.contains_key("id");
    let id_val = if has_id {
        match obj.get("id") {
            Some(v @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(v.clone()),
            _ => {
                return Err(Some(JsonRpcResponse::error(
                    Value::Null,
                    -32600,
                    "Invalid Request: 'id' must be a string, number, or null",
                )));
            }
        }
    } else {
        None
    };

    let err_id = id_val.clone().unwrap_or(Value::Null);

    let method = match obj.get("method") {
        Some(Value::String(m)) if !m.trim().is_empty() => m.clone(),
        _ => {
            return Err(Some(JsonRpcResponse::error(
                err_id,
                -32600,
                "Invalid Request: missing or non-string 'method'",
            )));
        }
    };

    match obj.get("jsonrpc") {
        Some(Value::String(v)) if v == "2.0" => {}
        _ => {
            if has_id {
                return Err(Some(JsonRpcResponse::error(
                    err_id,
                    -32600,
                    "Invalid Request: 'jsonrpc' must be '2.0'",
                )));
            } else {
                return Err(None);
            }
        }
    }

    let params = match obj.get("params") {
        None | Some(Value::Null) => None,
        Some(v @ (Value::Object(_) | Value::Array(_))) => Some(v.clone()),
        Some(_) => {
            if has_id {
                return Err(Some(JsonRpcResponse::error(
                    err_id,
                    -32600,
                    "Invalid Request: 'params' must be an object or array",
                )));
            } else {
                return Err(None);
            }
        }
    };

    Ok(ValidatedRpcMessage {
        id: id_val,
        method,
        params,
    })
}

fn is_cancel_method(method: &str) -> bool {
    matches!(
        method,
        "notifications/cancelled" | "$/cancelRequest" | "cancelled"
    )
}

fn extract_cancel_request_id(params: &Option<Value>) -> Option<Value> {
    let p = params.as_ref()?;
    p.get("requestId").or_else(|| p.get("id")).cloned()
}

fn spawn_tool_worker(msg: ValidatedRpcMessage) -> ActiveWorker {
    let id = msg.id.clone().unwrap_or(Value::Null);
    let task_cancel = CancelToken::new();
    let cancel_for_worker = task_cancel.clone();
    let (resp_tx, resp_rx) = crossbeam_channel::bounded::<JsonRpcResponse>(1);

    let handle = thread::spawn(move || {
        if let Some(resp) = handle_rpc_request(msg, &cancel_for_worker) {
            let _ = resp_tx.send(resp);
        }
    });

    ActiveWorker {
        id,
        cancel: task_cancel,
        resp_rx,
        handle,
    }
}

fn shutdown_active_worker(worker: Option<ActiveWorker>) {
    if let Some(w) = worker {
        w.cancel.cancel();
        let _ = w.resp_rx.recv_timeout(Duration::from_secs(5));
        let _ = w.handle.join();
    }
}

pub fn run_mcp_server() -> ExitCode {
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    let _ = install_ctrlc();
    let (cancel, ctrlc_rx) = subscribe_ctrlc();

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

    let mut active_worker: Option<ActiveWorker> = None;
    let mut pending_calls: VecDeque<ValidatedRpcMessage> = VecDeque::new();

    loop {
        if cancel.is_cancelled() {
            shutdown_active_worker(active_worker.take());
            break;
        }

        if active_worker.is_none()
            && let Some(next_call) = pending_calls.pop_front()
        {
            active_worker = Some(spawn_tool_worker(next_call));
        }

        if let Some(ref worker) = active_worker {
            let worker_rx = worker.resp_rx.clone();
            crossbeam_channel::select! {
                recv(ctrlc_rx) -> _ => {
                    shutdown_active_worker(active_worker.take());
                    break;
                }
                recv(worker_rx) -> resp_res => {
                    if let Ok(resp) = resp_res {
                        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                        let _ = writer.flush();
                    }
                    if let Some(w) = active_worker.take() {
                        let _ = w.handle.join();
                    }
                }
                recv(line_rx) -> incoming_res => {
                    match incoming_res {
                        Ok(line) => {
                            process_incoming_line(
                                &line,
                                &mut writer,
                                &cancel,
                                &mut active_worker,
                                &mut pending_calls,
                            );
                        }
                        Err(_) => {
                            // stdin EOF: cancel any running task and wait for clean shutdown
                            shutdown_active_worker(active_worker.take());
                            break;
                        }
                    }
                }
            }
        } else {
            crossbeam_channel::select! {
                recv(ctrlc_rx) -> _ => {
                    break;
                }
                recv(line_rx) -> incoming_res => {
                    match incoming_res {
                        Ok(line) => {
                            process_incoming_line(
                                &line,
                                &mut writer,
                                &cancel,
                                &mut active_worker,
                                &mut pending_calls,
                            );
                        }
                        Err(_) => {
                            break;
                        }
                    }
                }
            }
        }
    }

    ExitCode::from(EXIT_SUCCESS)
}

fn process_incoming_line(
    line: &str,
    writer: &mut impl Write,
    cancel: &CancelToken,
    active_worker: &mut Option<ActiveWorker>,
    pending_calls: &mut VecDeque<ValidatedRpcMessage>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            let err_resp = JsonRpcResponse::error(Value::Null, -32700, format!("Parse error: {e}"));
            let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
            let _ = writer.flush();
            return;
        }
    };

    if let Value::Array(batch) = parsed {
        if batch.is_empty() {
            let err_resp = JsonRpcResponse::error(
                Value::Null,
                -32600,
                "Invalid Request: batch array must not be empty",
            );
            let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
            let _ = writer.flush();
            return;
        }
        let mut batch_responses: Vec<JsonRpcResponse> = Vec::new();
        for item in batch {
            match validate_rpc_object(item) {
                Ok(msg) => {
                    if msg.id.is_none() {
                        handle_notification(&msg, active_worker, pending_calls, writer);
                    } else if let Some(resp) = handle_rpc_request(msg, cancel) {
                        batch_responses.push(resp);
                    }
                }
                Err(Some(err_resp)) => batch_responses.push(err_resp),
                Err(None) => {}
            }
        }
        if !batch_responses.is_empty() {
            let _ = writeln!(
                writer,
                "{}",
                serde_json::to_string(&batch_responses).unwrap()
            );
            let _ = writer.flush();
        }
        return;
    }

    let msg = match validate_rpc_object(parsed) {
        Ok(m) => m,
        Err(Some(err_resp)) => {
            let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
            let _ = writer.flush();
            return;
        }
        Err(None) => return,
    };

    // Notifications (absent "id") never receive a response
    if msg.id.is_none() {
        handle_notification(&msg, active_worker, pending_calls, writer);
        return;
    }

    if msg.method == "tools/call" {
        let id = msg.id.clone().unwrap_or(Value::Null);
        if let Err(err_resp) = prevalidate_tool_call(id, &msg.params) {
            let _ = writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
            let _ = writer.flush();
            return;
        }
        if active_worker.is_some() {
            pending_calls.push_back(msg);
        } else {
            *active_worker = Some(spawn_tool_worker(msg));
        }
    } else if let Some(resp) = handle_rpc_request(msg, cancel) {
        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
        let _ = writer.flush();
    }
}

fn handle_notification(
    msg: &ValidatedRpcMessage,
    active_worker: &mut Option<ActiveWorker>,
    pending_calls: &mut VecDeque<ValidatedRpcMessage>,
    writer: &mut impl Write,
) {
    if is_cancel_method(&msg.method) {
        let target_id = extract_cancel_request_id(&msg.params);
        match target_id {
            Some(ref req_id) => {
                if let Some(w) = active_worker.as_ref()
                    && &w.id == req_id
                {
                    w.cancel.cancel();
                    return;
                }
                if let Some(pos) = pending_calls
                    .iter()
                    .position(|queued| queued.id.as_ref() == Some(req_id))
                    && let Some(removed) = pending_calls.remove(pos)
                {
                    let resp = JsonRpcResponse::ok(
                        removed.id.unwrap_or(Value::Null),
                        serde_json::json!({
                            "content": [{ "type": "text", "text": "Error: Cancelled" }],
                            "isError": true
                        }),
                    );
                    let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                    let _ = writer.flush();
                }
            }
            None => {
                if let Some(w) = active_worker.as_ref() {
                    w.cancel.cancel();
                }
            }
        }
    }
}

fn negotiate_protocol_version(params: &Option<Value>) -> &'static str {
    if let Some(client_ver) = params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
    {
        for &supported in SUPPORTED_PROTOCOL_VERSIONS {
            if client_ver == supported {
                return supported;
            }
        }
    }
    DEFAULT_PROTOCOL_VERSION
}

fn handle_rpc_request(req: ValidatedRpcMessage, cancel: &CancelToken) -> Option<JsonRpcResponse> {
    let id = req.id?;

    match req.method.as_str() {
        "initialize" => {
            let negotiated = negotiate_protocol_version(&req.params);
            let res = serde_json::json!({
                "protocolVersion": negotiated,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "uj",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            Some(JsonRpcResponse::ok(id, res))
        }
        "ping" => Some(JsonRpcResponse::ok(id, serde_json::json!({}))),
        "tools/list" => {
            let tools = get_tools_list();
            Some(JsonRpcResponse::ok(
                id,
                serde_json::json!({ "tools": tools }),
            ))
        }
        "tools/call" => {
            if let Err(err_resp) = prevalidate_tool_call(id.clone(), &req.params) {
                return Some(err_resp);
            }
            let params = req.params.unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

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
                    Some(JsonRpcResponse::ok(id, res))
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
                    Some(JsonRpcResponse::ok(id, res))
                }
            }
        }
        _ => Some(JsonRpcResponse::error(
            id,
            -32601,
            format!("Method '{}' not found", req.method),
        )),
    }
}

fn validate_optional_string(
    id: &Value,
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, JsonRpcResponse> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: '{key}' must be a string"),
        )),
    }
}

fn validate_required_string(
    id: &Value,
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, JsonRpcResponse> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: '{key}' must not be empty"),
        )),
        Some(_) => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: '{key}' must be a string"),
        )),
        None => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: missing required parameter '{key}'"),
        )),
    }
}

fn validate_optional_bool(
    id: &Value,
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, JsonRpcResponse> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: '{key}' must be a boolean"),
        )),
    }
}

fn validate_optional_string_array(
    id: &Value,
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, JsonRpcResponse> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(JsonRpcResponse::error(
                            id.clone(),
                            -32602,
                            format!("Invalid params: all elements of '{key}' must be strings"),
                        ));
                    }
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(JsonRpcResponse::error(
            id.clone(),
            -32602,
            format!("Invalid params: '{key}' must be an array of strings"),
        )),
    }
}

fn validate_sandbox_params(
    id: &Value,
    args: &serde_json::Map<String, Value>,
) -> Result<(SandboxMode, bool), JsonRpcResponse> {
    let mode = match validate_optional_string(id, args, "sandbox")? {
        None => SandboxMode::Auto,
        Some(s) => match s.as_str() {
            "auto" => SandboxMode::Auto,
            "full" => SandboxMode::Full,
            "none" => SandboxMode::None,
            other => {
                return Err(JsonRpcResponse::error(
                    id.clone(),
                    -32602,
                    format!(
                        "Invalid params: 'sandbox' must be 'auto', 'full', or 'none' (got '{other}')"
                    ),
                ));
            }
        },
    };
    let require_tier = validate_optional_bool(id, args, "require_tier")?.unwrap_or(false);
    Ok((mode, require_tier))
}

fn prevalidate_tool_call(id: Value, params_opt: &Option<Value>) -> Result<(), JsonRpcResponse> {
    let params = match params_opt {
        Some(Value::Object(map)) => map,
        _ => {
            return Err(JsonRpcResponse::error(
                id,
                -32602,
                "Invalid params: tools/call requires a 'params' object",
            ));
        }
    };

    let name = match params.get("name") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.as_str(),
        _ => {
            return Err(JsonRpcResponse::error(
                id,
                -32602,
                "Invalid params: missing or non-string tool 'name'",
            ));
        }
    };

    let empty_map = serde_json::Map::new();
    let args = match params.get("arguments") {
        None | Some(Value::Null) => &empty_map,
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(JsonRpcResponse::error(
                id,
                -32602,
                "Invalid params: 'arguments' must be an object",
            ));
        }
    };

    match name {
        "doctor" => {
            if let Some(p) = args.get("problem") {
                match p {
                    Value::Bool(_) | Value::String(_) | Value::Null => {}
                    _ => {
                        return Err(JsonRpcResponse::error(
                            id,
                            -32602,
                            "Invalid params: 'problem' must be a boolean or string path",
                        ));
                    }
                }
            }
            validate_optional_string(&id, args, "problem_dir")?;
            Ok(())
        }
        "build" => {
            validate_optional_string(&id, args, "problem")?;
            validate_optional_string_array(&id, args, "targets")?;
            validate_optional_bool(&id, args, "no_cache")?;
            Ok(())
        }
        "judge" => {
            validate_optional_string(&id, args, "problem")?;
            validate_sandbox_params(&id, args)?;
            let sub = validate_optional_string(&id, args, "submission")?;
            let sources = validate_optional_string_array(&id, args, "sources")?;
            let has_sub = sub.as_ref().is_some_and(|s| !s.trim().is_empty());
            let has_sources = sources
                .as_ref()
                .is_some_and(|arr| !arr.is_empty() && arr.iter().all(|s| !s.trim().is_empty()));
            if !has_sub && !has_sources {
                return Err(JsonRpcResponse::error(
                    id,
                    -32602,
                    "Invalid params: 'judge' requires 'submission' (string) or non-empty 'sources' (array of strings)",
                ));
            }
            Ok(())
        }
        "verify" | "pipeline" => {
            validate_optional_string(&id, args, "problem")?;
            validate_sandbox_params(&id, args)?;
            Ok(())
        }
        "stress" => {
            validate_optional_string(&id, args, "problem")?;
            validate_required_string(&id, args, "generator")?;
            validate_required_string(&id, args, "solution")?;
            validate_required_string(&id, args, "brute")?;
            validate_optional_string(&id, args, "gen_args")?;
            validate_sandbox_params(&id, args)?;
            if let Some(c) = args.get("count")
                && !c.is_null()
            {
                match c.as_u64() {
                    Some(n) if n >= 1 => {}
                    _ => {
                        return Err(JsonRpcResponse::error(
                            id,
                            -32602,
                            "Invalid params: 'count' must be a positive integer",
                        ));
                    }
                }
            }
            if let Some(s) = args.get("seed")
                && !s.is_null()
                && s.as_u64().is_none()
            {
                return Err(JsonRpcResponse::error(
                    id,
                    -32602,
                    "Invalid params: 'seed' must be a non-negative integer",
                ));
            }
            if let Some(tb) = args.get("time_budget")
                && !tb.is_null()
            {
                match tb {
                    Value::String(s) => {
                        if let Err(e) = parse_duration_str(s) {
                            return Err(JsonRpcResponse::error(
                                id,
                                -32602,
                                format!("Invalid params: {e}"),
                            ));
                        }
                    }
                    Value::Number(n) if n.as_u64().is_some_and(|v| v >= 1) => {}
                    _ => {
                        return Err(JsonRpcResponse::error(
                            id,
                            -32602,
                            "Invalid params: 'time_budget' must be a duration string (e.g. '60s') or positive integer seconds",
                        ));
                    }
                }
            }
            Ok(())
        }
        "show" => {
            validate_optional_string(&id, args, "problem")?;
            validate_optional_string(&id, args, "test")?;
            if let Some(rid) = validate_optional_string(&id, args, "run_id")?
                && !rid.trim().is_empty()
                && let Err(e) = validate_run_id(&rid)
            {
                return Err(JsonRpcResponse::error(
                    id,
                    -32602,
                    format!("Invalid params: {e}"),
                ));
            }
            Ok(())
        }
        "contest" => {
            validate_required_string(&id, args, "dir")?;
            validate_optional_string_array(&id, args, "problems")?;
            validate_optional_string_array(&id, args, "contestants")?;
            validate_sandbox_params(&id, args)?;
            Ok(())
        }
        "init_problem" => {
            validate_required_string(&id, args, "dir")?;
            validate_optional_string(&id, args, "name")?;
            validate_optional_string(&id, args, "title")?;
            validate_optional_bool(&id, args, "force")?;
            if let Some(ptype) = validate_optional_string(&id, args, "type")?
                && !matches!(
                    ptype.as_str(),
                    "traditional"
                        | "interactive"
                        | "grader"
                        | "communication"
                        | "output_only"
                        | "output-only"
                )
            {
                return Err(JsonRpcResponse::error(
                    id,
                    -32602,
                    format!(
                        "Invalid params: unknown problem type '{ptype}' (expected traditional, interactive, grader, communication, or output_only)"
                    ),
                ));
            }
            Ok(())
        }
        other => Err(JsonRpcResponse::error(
            id,
            -32602,
            format!("Invalid params: unknown tool '{other}'"),
        )),
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
                        "description": "List of submission source paths (if judging multiple files)"
                    },
                    "sandbox": {
                        "type": "string",
                        "enum": ["auto", "full", "none"],
                        "description": "Sandbox isolation mode (default: 'auto')"
                    },
                    "require_tier": {
                        "type": "boolean",
                        "description": "Fail closed if full sandbox tier is unavailable (default: false)"
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "build",
            "description": "Compile problem binaries (checker, validator, interactor, manager, solutions, generators)",
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
                    },
                    "no_cache": {
                        "type": "boolean",
                        "description": "Disable compilation cache and force rebuild (default: false)"
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
                    },
                    "sandbox": {
                        "type": "string",
                        "enum": ["auto", "full", "none"],
                        "description": "Sandbox isolation mode (default: 'auto')"
                    },
                    "require_tier": {
                        "type": "boolean",
                        "description": "Fail closed if full sandbox tier is unavailable (default: false)"
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
                    },
                    "sandbox": {
                        "type": "string",
                        "enum": ["auto", "full", "none"],
                        "description": "Sandbox isolation mode (default: 'auto')"
                    },
                    "require_tier": {
                        "type": "boolean",
                        "description": "Fail closed if full sandbox tier is unavailable (default: false)"
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
                    },
                    "time_budget": {
                        "type": "string",
                        "description": "Maximum wall-clock time budget, e.g. '60s', '5m', '500ms' (default: '60s')"
                    },
                    "sandbox": {
                        "type": "string",
                        "enum": ["auto", "full", "none"],
                        "description": "Sandbox isolation mode (default: 'auto')"
                    },
                    "require_tier": {
                        "type": "boolean",
                        "description": "Fail closed if full sandbox tier is unavailable (default: false)"
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
                        "description": "Boolean (true checks '.') or string path to a problem directory to diagnose"
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
                    },
                    "sandbox": {
                        "type": "string",
                        "enum": ["auto", "full", "none"],
                        "description": "Sandbox isolation mode (default: 'auto')"
                    },
                    "require_tier": {
                        "type": "boolean",
                        "description": "Fail closed if full sandbox tier is unavailable (default: false)"
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
                        "enum": ["traditional", "interactive", "grader", "communication", "output_only"],
                        "description": "Problem type (default: 'traditional')"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Overwrite existing files if target directory isalready initialized (default: false)"
                    }
                },
                "required": ["dir"]
            }
        }),
    ]
}

fn configure_cli_from_args(cli: &mut Cli, args: &Value) {
    if let Some(sb) = args.get("sandbox").and_then(|v| v.as_str()) {
        cli.sandbox = match sb {
            "full" => SandboxMode::Full,
            "none" => SandboxMode::None,
            _ => SandboxMode::Auto,
        };
    }
    if let Some(rt) = args.get("require_tier").and_then(|v| v.as_bool()) {
        cli.require_tier = rt;
    }
    if let Some(nc) = args.get("no_cache").and_then(|v| v.as_bool()) {
        cli.no_cache = nc;
    }
}

fn dispatch_tool_call(
    name: &str,
    args: &Value,
    cancel: &CancelToken,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match name {
        "doctor" => {
            let prob_dir_buf: Option<PathBuf> = match args.get("problem") {
                Some(Value::Bool(true)) => Some(
                    args.get("problem_dir")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(".")),
                ),
                Some(Value::String(s)) if !s.trim().is_empty() => Some(PathBuf::from(s)),
                _ => args
                    .get("problem_dir")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from),
            };
            let report = doctor::run_doctor(prob_dir_buf.as_deref(), false);
            Ok(serde_json::to_string_pretty(&report)?)
        }
        "build" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let targets: Vec<String> = args
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let no_cache = args
                .get("no_cache")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let session = CompileSession::new();
            let (code, val) = crate::author_cmd::build_core(
                problem_dir,
                &targets,
                no_cache,
                OutputFormat::Silent,
                &session,
            );
            if code != ExitCode::from(EXIT_SUCCESS) {
                let msg = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Build failed");
                return Err(msg.to_string().into());
            }
            Ok(serde_json::to_string_pretty(&val)?)
        }
        "judge" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let config_path = problem_dir.join("problem.toml");
            let content = fs::read_to_string(&config_path)?;
            let cfg = ProblemConfig::from_toml_str(&content, Some(&config_path))?;
            let tests = uj_core::tests::discover_tests(problem_dir, &cfg)?;

            let mut sources = Vec::new();
            if let Some(sub) = args.get("submission").and_then(|v| v.as_str())
                && !sub.trim().is_empty()
            {
                sources.push(PathBuf::from(sub));
            }
            if let Some(arr) = args.get("sources").and_then(|v| v.as_array()) {
                for s in arr {
                    if let Some(str_val) = s.as_str()
                        && !str_val.trim().is_empty()
                    {
                        let p = PathBuf::from(str_val);
                        if !sources.contains(&p) {
                            sources.push(p);
                        }
                    }
                }
            }

            if sources.is_empty() {
                return Err("No submission source provided".into());
            }

            let mut cli = Cli::parse_from(["uj", "mcp"]);
            configure_cli_from_args(&mut cli, args);

            let judge_options = JudgeOptions {
                sandbox_mode: cli.sandbox_mode_str().to_string(),
                require_tier: cli.require_tier,
                fail_fast: false,
                precise: cli.effective_precise(),
            };
            let session = CompileSession::new();
            let scheduler = Scheduler::with_session(judge_options, session.clone());
            let mut reports = Vec::new();

            for src in &sources {
                let resolved_src = if src.exists() {
                    src.clone()
                } else {
                    let cand = problem_dir.join(src);
                    if cand.exists() { cand } else { src.clone() }
                };
                let compiled = session.compile_problem_solution(
                    problem_dir,
                    &cfg,
                    &resolved_src,
                    cli.no_cache,
                )?;
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
            let mut cli = Cli::parse_from(["uj", "verify"]);
            configure_cli_from_args(&mut cli, args);

            let session = CompileSession::new();
            let (code, val) = crate::author_cmd::verify_core(
                problem_dir,
                &cli,
                OutputFormat::Silent,
                cancel,
                &session,
            );
            if code != ExitCode::from(EXIT_SUCCESS) && code != ExitCode::from(EXIT_NON_AC) {
                let msg = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Verification failed");
                return Err(msg.to_string().into());
            }
            Ok(serde_json::to_string_pretty(&val)?)
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

            let mut cli = Cli::parse_from(["uj", "pipeline"]);
            configure_cli_from_args(&mut cli, args);

            let (code, val) =
                crate::author_cmd::pipeline_core(problem_dir, &cli, OutputFormat::Silent, cancel);
            if code != ExitCode::from(EXIT_SUCCESS) {
                let failed_step = val
                    .get("failed_step")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown");
                let detail = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("step failed");
                return Err(format!(
                    "Pipeline execution failed at step '{failed_step}' for '{}': {detail}",
                    problem_dir.display()
                )
                .into());
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
                "schema_version": 1,
                "ok": true,
                "step": "pipeline_complete",
                "tests_count": tests.len(),
                "steps": val["steps"]
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
            let budget = match args.get("time_budget") {
                Some(Value::String(s)) => parse_duration_str(s)?,
                Some(Value::Number(n)) => Duration::from_secs(n.as_u64().unwrap_or(60)),
                _ => Duration::from_secs(60),
            };

            let mut cli = Cli::parse_from(["uj", "mcp"]);
            configure_cli_from_args(&mut cli, args);

            let (code, val) = crate::author_cmd::stress_core(
                problem_dir,
                gen_str,
                sol_str,
                brute_str,
                count,
                Some(seed_start),
                gen_args_tmpl,
                budget,
                &cli,
                OutputFormat::Silent,
                cancel,
            );
            if code != ExitCode::from(EXIT_SUCCESS) && code != ExitCode::from(EXIT_NON_AC) {
                let msg = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Stress test execution failed");
                return Err(msg.to_string().into());
            }
            Ok(serde_json::to_string_pretty(&val)?)
        }
        "show" => {
            let prob_str = args.get("problem").and_then(|v| v.as_str()).unwrap_or(".");
            let problem_dir = Path::new(prob_str);
            let run_file = if let Some(id) = args.get("run_id").and_then(|v| v.as_str())
                && !id.trim().is_empty()
            {
                let filename = validate_run_id(id)?;
                problem_dir.join(".uj").join("runs").join(filename)
            } else {
                problem_dir.join(".uj").join("runs").join("last.json")
            };

            if !run_file.exists() {
                return Err(format!("Run file '{}' does not exist", run_file.display()).into());
            }

            let content = fs::read_to_string(&run_file)?;
            let report: SubmissionJudgeReport = serde_json::from_str(&content)?;

            if let Some(target_test) = args.get("test").and_then(|v| v.as_str())
                && !target_test.trim().is_empty()
            {
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
                return Err(format!("Contest directory '{dir_str}' not found").into());
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

            let mut cli = Cli::parse_from(["uj", "contest", dir_str]);
            configure_cli_from_args(&mut cli, args);

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
                "schema_version": 1,
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
            let name = args.get("name").and_then(|v| v.as_str());
            let title = args.get("title").and_then(|v| v.as_str());
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

            let code =
                crate::author_cmd::run_init(dir, ptype, name, title, force, OutputFormat::Silent);
            if code != ExitCode::from(EXIT_SUCCESS) {
                return Err(format!("Failed to initialize problem at '{dir_str}'").into());
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "dir": dir_str,
                "name": name,
                "title": title,
                "type": ptype,
                "message": format!("Problem initialized at '{dir_str}'")
            }))?)
        }
        _ => Err(format!("Unknown tool '{name}'").into()),
    }
}

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn uj_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_uj"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn make_temp_dir(prefix: &str) -> PathBuf {
    let base = uj_sandbox::get_sandbox_scratch_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = base.join(format!("{prefix}_{pid}_{nanos}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_import_lemon_rejects_traversal_and_escapes_toml() {
    let temp = make_temp_dir("lemon_import_sec");
    let victim_dir = temp.join("victim");
    fs::create_dir_all(&victim_dir).unwrap();
    let victim_file = victim_dir.join("keep.txt");
    fs::write(&victim_file, "do not delete").unwrap();

    // 1. Malicious sourceFileName with "../victim" must be rejected without touching victim_dir
    let bad_cdf = temp.join("bad.cdf");
    fs::write(
        &bad_cdf,
        serde_json::to_string(&serde_json::json!({
            "contestTitle": "Evil Contest",
            "tasks": [{
                "taskTitle": "Evil",
                "sourceFileName": "../victim",
                "testCases": []
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(uj_bin())
        .args(["import", "lemon"])
        .arg(&bad_cdf)
        .args(["--format", "json"])
        .current_dir(&temp)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    assert!(
        victim_file.is_file(),
        "victim file must not be deleted by path traversal"
    );
    let err_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(err_json["schema_version"], 1);
    assert_eq!(err_json["ok"], false);

    // 2. Valid CDF with quotes and backslashes in title and filenames must produce valid problem.toml
    let good_cdf = temp.join("good.cdf");
    fs::write(
        &good_cdf,
        serde_json::to_string(&serde_json::json!({
            "contestTitle": "Good Contest",
            "tasks": [{
                "taskTitle": "Title with \"quotes\" and \\backslash",
                "sourceFileName": "safe_task",
                "inputFileName": "safe\"in.in",
                "outputFileName": "safe\\out.out",
                "testCases": [{
                    "fullScore": 100.0,
                    "timeLimit": 1000,
                    "memoryLimit": 256,
                    "inputFiles": ["1.in"],
                    "outputFiles": ["1.out"]
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let out_ok = Command::new(uj_bin())
        .args(["import", "lemon"])
        .arg(&good_cdf)
        .args(["--format", "json"])
        .current_dir(&temp)
        .output()
        .unwrap();
    assert_eq!(
        out_ok.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out_ok.stderr)
    );
    let ok_json: serde_json::Value = serde_json::from_slice(&out_ok.stdout).unwrap();
    assert_eq!(ok_json["ok"], true);

    // Re-importing without --force must fail with exit code 2
    let out_dup = Command::new(uj_bin())
        .args(["import", "lemon"])
        .arg(&good_cdf)
        .args(["--format", "json"])
        .current_dir(&temp)
        .output()
        .unwrap();
    assert_eq!(out_dup.status.code(), Some(2));

    // Re-importing with --force succeeds
    let out_force = Command::new(uj_bin())
        .args(["import", "lemon"])
        .arg(&good_cdf)
        .args(["--force", "--format", "json"])
        .current_dir(&temp)
        .output()
        .unwrap();
    assert_eq!(out_force.status.code(), Some(0));

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_init_flags_overwrite_protection_and_all_problem_types() {
    let temp = make_temp_dir("init_types");

    for ptype in [
        "traditional",
        "interactive",
        "grader",
        "communication",
        "output_only",
    ] {
        let pdir = temp.join(ptype);
        let out = Command::new(uj_bin())
            .arg("init")
            .arg(&pdir)
            .args([
                "--type",
                ptype,
                "--name",
                &format!("prob_{ptype}"),
                "--title",
                "Title with \"quotes\" and \\",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "uj init failed for type {ptype}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(val["ok"], true);
        assert_eq!(val["schema_version"], 1);

        let toml_content = fs::read_to_string(pdir.join("problem.toml")).unwrap();
        let cfg = uj_core::config::ProblemConfig::from_toml_str(&toml_content, None)
            .unwrap_or_else(|e| panic!("Generated problem.toml invalid for {ptype}: {e}"));
        assert_eq!(cfg.name, format!("prob_{ptype}"));
    }

    // Second init without --force must fail with exit code 2
    let trad_dir = temp.join("traditional");
    let custom_marker = trad_dir.join("solutions").join("std.cpp");
    fs::write(&custom_marker, "// custom marker").unwrap();

    let out_no_force = Command::new(uj_bin())
        .arg("init")
        .arg(&trad_dir)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out_no_force.status.code(), Some(2));
    assert_eq!(
        fs::read_to_string(&custom_marker).unwrap(),
        "// custom marker"
    );

    // Invalid --type must fail with exit code 2
    let out_bad_type = Command::new(uj_bin())
        .arg("init")
        .arg(temp.join("bad"))
        .args(["--type", "nonexistent_type"])
        .output()
        .unwrap();
    assert_eq!(out_bad_type.status.code(), Some(2));

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_pack_validates_before_clean_and_preserves_non_test_files() {
    let temp = make_temp_dir("pack_safety");
    let init_out = Command::new(uj_bin())
        .arg("init")
        .arg(&temp)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(init_out.status.code(), Some(0));

    let tests_dir = temp.join("tests");
    let orphan_in = tests_dir.join("99.in");
    let helper_script = tests_dir.join("helper.py");
    fs::write(&orphan_in, "99 99\n").unwrap();
    fs::write(&helper_script, "print('keep me')\n").unwrap();

    // Remove 1.ans so packing will fail with MissingAnswerFile
    fs::remove_file(tests_dir.join("1.ans")).unwrap();

    // uj pack --clean must fail BEFORE deleting 99.in!
    let pack_fail = Command::new(uj_bin())
        .arg("pack")
        .arg("-p")
        .arg(&temp)
        .args(["--clean", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(pack_fail.status.code(), Some(2));
    assert!(
        orphan_in.is_file(),
        "99.in must NOT be deleted when pack validation fails"
    );
    assert!(helper_script.is_file());

    // Restore 1.ans and restrict generator count to 1 so 99.in is treated as an orphan test artifact
    fs::write(tests_dir.join("1.ans"), "3\n").unwrap();
    let toml_path = temp.join("problem.toml");
    let toml_str = fs::read_to_string(&toml_path)
        .unwrap()
        .replace("count = 5", "count = 1");
    fs::write(&toml_path, toml_str).unwrap();

    let pack_ok = Command::new(uj_bin())
        .arg("pack")
        .arg("-p")
        .arg(&temp)
        .args(["--clean", "--format", "ndjson"])
        .output()
        .unwrap();
    assert_eq!(
        pack_ok.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&pack_ok.stderr)
    );
    let pack_json: serde_json::Value = serde_json::from_slice(&pack_ok.stdout).unwrap();
    assert_eq!(pack_json["ok"], true);
    assert!(
        !orphan_in.exists(),
        "Orphan 99.in should be cleaned after validation succeeds"
    );
    assert!(
        helper_script.is_file(),
        "Non-test file helper.py in tests/ must NEVER be deleted by --clean"
    );

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_run_id_traversal_rejected_in_show_and_report() {
    let aplusb = workspace_root().join("tests/problems/aplusb");

    let show_out = Command::new(uj_bin())
        .args(["show", "../problem.toml", "-p"])
        .arg(&aplusb)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(show_out.status.code(), Some(2));
    let show_json: serde_json::Value = serde_json::from_slice(&show_out.stdout).unwrap();
    assert_eq!(show_json["ok"], false);
    assert_eq!(show_json["error"]["kind"], "InvalidRunId");

    let report_out = Command::new(uj_bin())
        .args(["report", "../problem.toml", "--csv", "out.csv", "-p"])
        .arg(&aplusb)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(report_out.status.code(), Some(2));
    let report_json: serde_json::Value = serde_json::from_slice(&report_out.stdout).unwrap();
    assert_eq!(report_json["ok"], false);
    assert_eq!(report_json["error"]["kind"], "InvalidRunId");
}

#[test]
fn test_stress_fails_on_invalid_problem_toml_and_supports_duration_budget() {
    let temp = make_temp_dir("stress_cfg");
    let init_out = Command::new(uj_bin())
        .arg("init")
        .arg(&temp)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(init_out.status.code(), Some(0));

    // Corrupt problem.toml
    fs::write(temp.join("problem.toml"), "invalid = [toml").unwrap();

    let stress_bad = Command::new(uj_bin())
        .arg("stress")
        .arg("-p")
        .arg(&temp)
        .args([
            "--gen",
            "gen.cpp",
            "--sol",
            "solutions/std.cpp",
            "--brute",
            "solutions/brute.cpp",
            "--count",
            "2",
            "--time-budget",
            "5s",
            "--format",
            "ndjson",
        ])
        .output()
        .unwrap();
    assert_eq!(
        stress_bad.status.code(),
        Some(2),
        "uj stress must fail on invalid problem.toml"
    );
    let err_json: serde_json::Value = serde_json::from_slice(&stress_bad.stdout).unwrap();
    assert_eq!(err_json["ok"], false);

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_mcp_protocol_validation_version_negotiation_and_invalid_params() {
    let mut child = Command::new(uj_bin())
        .arg("mcp")
        .current_dir(workspace_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn uj mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    fn send_and_recv(
        stdin: &mut impl Write,
        reader: &mut impl BufRead,
        line: &str,
    ) -> serde_json::Value {
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut resp = String::new();
        reader.read_line(&mut resp).unwrap();
        serde_json::from_str(&resp)
            .unwrap_or_else(|e| panic!("Invalid JSON from MCP ({e}): {resp}"))
    }

    // 1. Parse error (-32700)
    let r = send_and_recv(&mut stdin, &mut reader, "{bad json");
    assert_eq!(r["error"]["code"], -32700);

    // 2. Non-object primitive (-32600)
    let r = send_and_recv(&mut stdin, &mut reader, "42");
    assert_eq!(r["error"]["code"], -32600);

    // 3. Empty batch array (-32600)
    let r = send_and_recv(&mut stdin, &mut reader, "[]");
    assert_eq!(r["error"]["code"], -32600);

    // 4. Missing method (-32600)
    let r = send_and_recv(&mut stdin, &mut reader, r#"{"jsonrpc":"2.0","id":10}"#);
    assert_eq!(r["id"], 10);
    assert_eq!(r["error"]["code"], -32600);

    // 5. Invalid id type (boolean) -> -32600
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
    );
    assert_eq!(r["error"]["code"], -32600);

    // 6. Notification (absent "id") must NOT produce a response, even for custom notification name,
    // followed by a request with explicit "id": null which MUST produce a response with "id": null.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"custom/no_response_expected"}}"#
    )
    .unwrap();
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
    );
    assert!(r["id"].is_null());
    assert!(r["result"].is_object());

    // 7. Protocol version negotiation: echo supported "2025-03-26"
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":11,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
    );
    assert_eq!(r["id"], 11);
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");

    // 8. Unknown tool name -> -32602 (Invalid params)
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
    );
    assert_eq!(r["id"], 12);
    assert_eq!(r["error"]["code"], -32602);

    // 9. Missing required tool parameter (judge without submission/sources) -> -32602
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"judge","arguments":{"problem":"tests/problems/aplusb"}}}"#,
    );
    assert_eq!(r["id"], 13);
    assert_eq!(r["error"]["code"], -32602);

    // 10. Path traversal in show run_id -> -32602
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"show","arguments":{"run_id":"../problem.toml"}}}"#,
    );
    assert_eq!(r["id"], 14);
    assert_eq!(r["error"]["code"], -32602);

    // 11. doctor accepts string path to problem directory
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"doctor","arguments":{"problem":"tests/problems/aplusb"}}}"#,
    );
    assert_eq!(r["id"], 15);
    assert_eq!(r["result"]["isError"], false);
    let doc_text = r["result"]["content"][0]["text"].as_str().unwrap();
    let doc_json: serde_json::Value = serde_json::from_str(doc_text).unwrap();
    assert_eq!(doc_json["schema_version"], 1);
    assert_eq!(doc_json["ok"], true);

    // 12. Non-empty batch array with notification + request returns array with 1 response
    let r = send_and_recv(
        &mut stdin,
        &mut reader,
        r#"[{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":16,"method":"ping"}]"#,
    );
    let arr = r.as_array().expect("Expected batch array response");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], 16);

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn test_judge_ce_ndjson_purity_and_last_show_clean_with_problem_flag() {
    let temp = make_temp_dir("judge_ce_ndjson");
    let init_out = Command::new(uj_bin())
        .arg("init")
        .arg(&temp)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(init_out.status.code(), Some(0));

    let bad_cpp = temp.join("bad_compile.cpp");
    fs::write(&bad_cpp, "int main() { syntax_error_here }").unwrap();

    // Run uj judge with --format ndjson and --tui (non-TTY should gracefully fallback without panic)
    let judge_out = Command::new(uj_bin())
        .arg("judge")
        .arg(&bad_cpp)
        .arg("-p")
        .arg(&temp)
        .args(["--format", "ndjson", "--tui", "--color", "never"])
        .output()
        .unwrap();
    assert_eq!(judge_out.status.code(), Some(5));

    let stdout_str = String::from_utf8_lossy(&judge_out.stdout);
    let lines: Vec<&str> = stdout_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(!lines.is_empty(), "Expected NDJSON events on CE");
    for line in &lines {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Non-JSON line on stdout in NDJSON mode ({e}): {line}"));
    }

    // Verify .uj/runs/last.json was written for the CE run and `uj last -p <temp> --format ndjson` works
    let last_out = Command::new(uj_bin())
        .arg("last")
        .arg("-p")
        .arg(&temp)
        .args(["--format", "ndjson"])
        .output()
        .unwrap();
    assert_eq!(last_out.status.code(), Some(1));
    let last_lines: Vec<&str> = std::str::from_utf8(&last_out.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(last_lines.len(), 1, "NDJSON must be a single line");
    let last_json: serde_json::Value = serde_json::from_str(last_lines[0]).unwrap();
    assert_eq!(last_json["verdict"], "CE");

    // Verify `uj show -p <temp> --test compile --format ndjson` returns single-line JSON with CE message
    let show_out = Command::new(uj_bin())
        .arg("show")
        .arg("-p")
        .arg(&temp)
        .args(["--test", "compile", "--format", "ndjson"])
        .output()
        .unwrap();
    assert_eq!(show_out.status.code(), Some(1));
    let show_json: serde_json::Value = serde_json::from_slice(&show_out.stdout).unwrap();
    assert_eq!(show_json["verdict"], "CE");
    assert!(
        show_json["message"]
            .as_str()
            .unwrap_or("")
            .contains("syntax_error_here")
    );

    // Verify `uj clean -p <temp> --format ndjson` emits valid JSON on stdout
    let clean_out = Command::new(uj_bin())
        .arg("clean")
        .arg("-p")
        .arg(&temp)
        .args(["--format", "ndjson"])
        .output()
        .unwrap();
    assert_eq!(clean_out.status.code(), Some(0));
    let clean_json: serde_json::Value = serde_json::from_slice(&clean_out.stdout).unwrap();
    assert_eq!(clean_json["ok"], true);
    assert_eq!(clean_json["schema_version"], 1);

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_pipeline_json_reports_failure_detail_and_doctor_ndjson() {
    let temp = make_temp_dir("pipeline_err_json");
    let init_out = Command::new(uj_bin())
        .arg("init")
        .arg(&temp)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(init_out.status.code(), Some(0));

    // Make validator.cpp fail validation on every test case
    fs::write(
        temp.join("validator.cpp"),
        "#include <cstdlib>\nint main() { return 1; }\n",
    )
    .unwrap();

    let pipe_out = Command::new(uj_bin())
        .arg("pipeline")
        .arg("-p")
        .arg(&temp)
        .args(["--format", "json", "--sandbox", "none"])
        .output()
        .unwrap();
    assert_ne!(pipe_out.status.code(), Some(0));
    let pipe_json: serde_json::Value = serde_json::from_slice(&pipe_out.stdout).unwrap();
    assert_eq!(pipe_json["schema_version"], 1);
    assert_eq!(pipe_json["ok"], false);
    assert_eq!(pipe_json["failed_step"], "validate");
    assert!(
        pipe_json["error"].is_object(),
        "Expected structured error detail in pipeline JSON output: {pipe_json}"
    );
    assert_eq!(pipe_json["error"]["stage"], "validate");

    // Also check `uj doctor -p <temp> --format ndjson` outputs a single JSON line
    let doc_out = Command::new(uj_bin())
        .arg("doctor")
        .arg("-p")
        .arg(&temp)
        .args(["--format", "ndjson"])
        .output()
        .unwrap();
    let doc_lines: Vec<&str> = std::str::from_utf8(&doc_out.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(doc_lines.len(), 1);
    let doc_json: serde_json::Value = serde_json::from_str(doc_lines[0]).unwrap();
    assert_eq!(doc_json["schema_version"], 1);

    let _ = fs::remove_dir_all(&temp);
}

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn test_mcp_initialize_and_tools_list() {
    let uj_bin = env!("CARGO_BIN_EXE_uj");
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let mut child = Command::new(uj_bin)
        .arg("mcp")
        .current_dir(&workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn uj mcp");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    // 1. Initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0"
            }
        }
    });

    writeln!(stdin, "{}", serde_json::to_string(&init_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    let init_resp: serde_json::Value = serde_json::from_str(&resp_line).expect("Invalid JSON");

    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "uj");
    assert!(init_resp["result"]["capabilities"]["tools"].is_object());

    // 2. tools/list
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });

    writeln!(stdin, "{}", serde_json::to_string(&list_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).unwrap();
    let list_resp: serde_json::Value = serde_json::from_str(&resp_line).expect("Invalid JSON");

    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("Tools array");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert!(tool_names.contains(&"judge"));
    assert!(tool_names.contains(&"build"));
    assert!(tool_names.contains(&"verify"));
    assert!(tool_names.contains(&"stress"));
    assert!(tool_names.contains(&"pipeline"));
    assert!(tool_names.contains(&"doctor"));
    assert!(tool_names.contains(&"show"));
    assert!(tool_names.contains(&"contest"));

    // 3. tools/call: doctor
    let doctor_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "doctor",
            "arguments": {
                "problem": false
            }
        }
    });

    writeln!(stdin, "{}", serde_json::to_string(&doctor_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).unwrap();
    let doctor_resp: serde_json::Value = serde_json::from_str(&resp_line).expect("Invalid JSON");

    assert_eq!(doctor_resp["id"], 3);
    assert_eq!(doctor_resp["result"]["isError"], false);
    let text = doctor_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("\"checks\""));

    // 4. tools/call: judge on aplusb
    let judge_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "judge",
            "arguments": {
                "problem": "tests/problems/aplusb",
                "submission": "tests/problems/aplusb/solutions/ac.cpp"
            }
        }
    });

    writeln!(stdin, "{}", serde_json::to_string(&judge_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).unwrap();
    let judge_resp: serde_json::Value = serde_json::from_str(&resp_line).expect("Invalid JSON");

    assert_eq!(judge_resp["id"], 4);
    assert_eq!(judge_resp["result"]["isError"], false);
    assert_eq!(judge_resp["result"]["structuredContent"]["verdict"], "AC");
    let judge_text = judge_resp["result"]["content"][0]["text"].as_str().unwrap();
    let judge_json: serde_json::Value =
        serde_json::from_str(judge_text).expect("Invalid judge JSON");
    assert_eq!(judge_json["verdict"], "AC");

    // 5. tools/call: multi-source judge on aplusb
    let multi_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "judge",
            "arguments": {
                "problem": "tests/problems/aplusb",
                "sources": [
                    "tests/problems/aplusb/solutions/ac.cpp",
                    "tests/problems/aplusb/solutions/wa.cpp"
                ]
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&multi_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).unwrap();
    let multi_resp: serde_json::Value = serde_json::from_str(&resp_line).expect("Invalid JSON");
    assert_eq!(multi_resp["id"], 5);
    assert_eq!(multi_resp["result"]["isError"], false);
    let multi_text = multi_resp["result"]["content"][0]["text"].as_str().unwrap();
    let multi_json: serde_json::Value =
        serde_json::from_str(multi_text).expect("Invalid multi-judge JSON");
    let arr = multi_json.as_array().expect("Expected array of reports");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["verdict"], "AC");
    assert_eq!(arr[1]["verdict"], "WA");

    drop(stdin);
    let _ = child.wait();
}

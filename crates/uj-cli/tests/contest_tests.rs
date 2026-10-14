use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tempfile::tempdir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn uj_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(if cfg!(windows) { "uj.exe" } else { "uj" });
    path
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[test]
fn test_lemon_cdf_import() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let root = temp.path();

    // 1. Test valid LemonLime JSON .cdf import
    let cdf_content = serde_json::json!({
        "version": "1.0",
        "contestTitle": "Spring 2026 Invitational",
        "tasks": [
            {
                "problemTitle": "Addition Problem",
                "sourceFileName": "add",
                "inputFileName": "add.in",
                "outputFileName": "add.out",
                "standardInputCheck": true,
                "standardOutputCheck": true,
                "taskType": 0,
                "comparisonMode": 1,
                "testCases": [
                    {
                        "fullScore": 50.0,
                        "timeLimit": 1000,
                        "memoryLimit": 256,
                        "inputFiles": ["1.in"],
                        "outputFiles": ["1.out"]
                    },
                    {
                        "fullScore": 50.0,
                        "timeLimit": 1000,
                        "memoryLimit": 256,
                        "inputFiles": ["2.in", "1_lemon_SUbtaskDEPENDENCE_fLAg"],
                        "outputFiles": ["2.out"]
                    }
                ]
            }
        ]
    });

    let cdf_path = root.join("contest.cdf");
    fs::write(
        &cdf_path,
        serde_json::to_string_pretty(&cdf_content).unwrap(),
    )
    .unwrap();

    let import_res = Command::new(&uj)
        .arg("import")
        .arg("lemon")
        .arg(&cdf_path)
        .current_dir(root)
        .output()
        .expect("failed to execute uj import lemon");

    assert!(
        import_res.status.success(),
        "Import should succeed: {}",
        String::from_utf8_lossy(&import_res.stderr)
    );

    let generated_toml = root.join("problems").join("add").join("problem.toml");
    assert!(generated_toml.exists(), "problem.toml must be generated");

    let toml_str = fs::read_to_string(&generated_toml).unwrap();
    assert!(toml_str.contains("name = \"add\""));
    assert!(toml_str.contains("title = \"Addition Problem\""));
    assert!(toml_str.contains("time = \"1000ms\""));
    assert!(toml_str.contains("memory = \"256MB\""));
    assert!(toml_str.contains("type = \"builtin\""));
    assert!(toml_str.contains("name = \"wcmp\""));
    assert!(toml_str.contains("depends = [1]"));

    // 2. Test legacy binary .cdf rejection
    let legacy_cdf_path = root.join("legacy.cdf");
    let fake_binary = vec![0x00, 0x00, 0x01, 0xFF, 0xFE, 0xFD];
    fs::write(&legacy_cdf_path, &fake_binary).unwrap();

    let legacy_res = Command::new(&uj)
        .arg("--format")
        .arg("human")
        .arg("import")
        .arg("lemon")
        .arg(&legacy_cdf_path)
        .current_dir(root)
        .output()
        .expect("failed to execute uj import lemon for binary");

    assert!(
        !legacy_res.status.success(),
        "Binary .cdf should be rejected"
    );
    let combined_str = format!(
        "{}\n{}",
        String::from_utf8_lossy(&legacy_res.stdout),
        String::from_utf8_lossy(&legacy_res.stderr)
    );
    assert!(
        combined_str.contains("Legacy Lemon binary .cdf is not supported"),
        "Should provide helpful migration message"
    );
}

#[test]
fn test_contest_judging_and_leaderboard() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let contest_dir = temp.path();

    // 1. Setup problem inside <contest>/problems/aplusb
    let problem_dir = contest_dir.join("problems").join("aplusb");
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/problems/aplusb");
    copy_dir_all(&fixture_dir, &problem_dir).unwrap();

    // 2. Setup contestants in Lemon directory structure:
    // source/<player>/<problem>/<problem>.cpp
    let alice_dir = contest_dir.join("source").join("alice").join("aplusb");
    let bob_dir = contest_dir.join("source").join("bob").join("aplusb");
    let charlie_dir = contest_dir.join("source").join("charlie").join("aplusb");

    fs::create_dir_all(&alice_dir).unwrap();
    fs::create_dir_all(&bob_dir).unwrap();
    fs::create_dir_all(&charlie_dir).unwrap();

    fs::copy(
        problem_dir.join("solutions").join("ac.cpp"),
        alice_dir.join("aplusb.cpp"),
    )
    .unwrap();

    fs::copy(
        problem_dir.join("solutions").join("wa.cpp"),
        bob_dir.join("aplusb.cpp"),
    )
    .unwrap();

    fs::copy(
        problem_dir.join("solutions").join("ce.cpp"),
        charlie_dir.join("aplusb.cpp"),
    )
    .unwrap();

    // 3. Run uj contest
    let contest_output = Command::new(&uj)
        .arg("--format")
        .arg("human")
        .arg("contest")
        .arg(contest_dir)
        .output()
        .expect("failed to execute uj contest");

    assert!(
        contest_output.status.success(),
        "Contest execution failed: {}",
        String::from_utf8_lossy(&contest_output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&contest_output.stdout);
    assert!(stdout_str.contains("Contest Leaderboard"));
    assert!(stdout_str.contains("alice"));
    assert!(stdout_str.contains("bob"));
    assert!(stdout_str.contains("charlie"));

    // 4. Verify generated HTML and CSV contest reports
    let html_report = contest_dir.join("contest_report.html");
    let csv_report = contest_dir.join("contest_report.csv");

    assert!(html_report.exists(), "contest_report.html must exist");
    assert!(csv_report.exists(), "contest_report.csv must exist");

    let html_content = fs::read_to_string(&html_report).unwrap();
    assert!(html_content.contains("Contest Standings"));
    assert!(html_content.contains("alice"));
    assert!(html_content.contains("badge-ac"));
    assert!(html_content.contains("badge-wa"));

    let csv_content = fs::read_to_string(&csv_report).unwrap();
    assert!(csv_content.contains("submission,problem,test,verdict,time_ms,memory_kb,score"));
    assert!(csv_content.contains("alice - aplusb"));
    assert!(csv_content.contains(",AC,"));

    // 5. Test JSON output format for contest
    let json_output = Command::new(&uj)
        .arg("--format")
        .arg("json")
        .arg("contest")
        .arg(contest_dir)
        .output()
        .expect("failed to execute uj contest with json format");

    assert!(json_output.status.success());
    let json_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_output.stdout))
            .expect("Must be valid JSON");

    assert_eq!(json_val["ok"], true);
    assert_eq!(json_val["problems_count"], 1);
    assert_eq!(json_val["contestants_count"], 3);

    let standings = json_val["standings"].as_array().unwrap();
    assert_eq!(standings[0]["name"], "alice");
    assert_eq!(standings[0]["total_score"], 100.0);
    assert_eq!(standings[0]["rank"], 1);
}

#[test]
fn test_contest_ce_and_tle_tail() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let contest_dir = temp.path();

    let tail_fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contests/tail");
    if !tail_fixture.exists() {
        return;
    }
    copy_dir_all(&tail_fixture, contest_dir).unwrap();

    let json_output = Command::new(&uj)
        .arg("--format")
        .arg("json")
        .arg("contest")
        .arg(contest_dir)
        .output()
        .expect("failed to execute uj contest");

    assert!(
        json_output.status.success(),
        "Contest execution failed: {}",
        String::from_utf8_lossy(&json_output.stderr)
    );

    let json_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_output.stdout))
            .expect("Must be valid JSON");

    assert_eq!(json_val["ok"], true);
    assert_eq!(json_val["problems_count"], 2);
    assert_eq!(json_val["contestants_count"], 10);

    let standings = json_val["standings"].as_array().unwrap();

    // Check CE submission
    let ce1 = standings
        .iter()
        .find(|s| s["name"] == "ce1")
        .expect("ce1 not found");
    assert_eq!(ce1["total_score"], 0.0);
    assert_eq!(ce1["problems"]["aplusb"]["verdict"], "CE");
    assert_eq!(ce1["problems"]["slowsum"]["verdict"], "CE");

    // Check TLE submissions
    let tle1 = standings
        .iter()
        .find(|s| s["name"] == "tle1")
        .expect("tle1 not found");
    assert_eq!(tle1["problems"]["aplusb"]["verdict"], "AC");
    assert_eq!(tle1["problems"]["slowsum"]["verdict"], "TLE");

    let tle2 = standings
        .iter()
        .find(|s| s["name"] == "tle2")
        .expect("tle2 not found");
    assert_eq!(tle2["problems"]["aplusb"]["verdict"], "AC");
    assert_eq!(tle2["problems"]["slowsum"]["verdict"], "TLE");

    // Check AC submissions
    for name in &["ac1", "ac2", "ac3", "ac4", "ac5", "ac6"] {
        let ac = standings
            .iter()
            .find(|s| s["name"] == *name)
            .unwrap_or_else(|| panic!("{name} not found"));
        assert_eq!(
            ac["total_score"], 200.0,
            "{name} total score should be 200.0"
        );
        assert_eq!(ac["problems"]["aplusb"]["verdict"], "AC");
        assert_eq!(ac["problems"]["slowsum"]["verdict"], "AC");
    }
}

#[test]
fn test_contest_report_order_deterministic() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let tail_fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contests/tail");
    if !tail_fixture.exists() {
        return;
    }

    // Run 1: --jobs 1
    let temp1 = tempdir().unwrap();
    copy_dir_all(&tail_fixture, temp1.path()).unwrap();
    let out1 = Command::new(&uj)
        .arg("--jobs")
        .arg("1")
        .arg("--format")
        .arg("human")
        .arg("contest")
        .arg(temp1.path())
        .output()
        .expect("failed to execute uj contest with jobs 1");
    assert!(
        out1.status.success(),
        "Jobs 1 failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );

    // Run 2: --jobs 8
    let temp2 = tempdir().unwrap();
    copy_dir_all(&tail_fixture, temp2.path()).unwrap();
    let out2 = Command::new(&uj)
        .arg("--jobs")
        .arg("8")
        .arg("--format")
        .arg("human")
        .arg("contest")
        .arg(temp2.path())
        .output()
        .expect("failed to execute uj contest with jobs 8");
    assert!(
        out2.status.success(),
        "Jobs 8 failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let csv1 = fs::read_to_string(temp1.path().join("contest_report.csv")).unwrap();
    let csv2 = fs::read_to_string(temp2.path().join("contest_report.csv")).unwrap();

    // Strip time_ms (index 4) and memory_kb (index 5) columns:
    // "submission,problem,test,verdict,time_ms,memory_kb,score" -> "submission,problem,test,verdict,score"
    fn strip_dynamic_columns(csv: &str) -> Vec<String> {
        csv.lines()
            .map(|line| {
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() >= 7 {
                    format!(
                        "{},{},{},{},{}",
                        parts[0], parts[1], parts[2], parts[3], parts[6]
                    )
                } else {
                    line.to_string()
                }
            })
            .collect()
    }

    let rows1 = strip_dynamic_columns(&csv1);
    let rows2 = strip_dynamic_columns(&csv2);

    assert_eq!(rows1.len(), rows2.len(), "Row count must be identical");
    for (idx, (r1, r2)) in rows1.iter().zip(rows2.iter()).enumerate() {
        assert_eq!(r1, r2, "Row {idx} mismatch between --jobs 1 and --jobs 8");
    }
}

#[test]
fn test_contest_precise_after_pool() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let contest_dir = temp.path();

    let tail_fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contests/tail");
    if !tail_fixture.exists() {
        return;
    }
    copy_dir_all(&tail_fixture, contest_dir).unwrap();

    let out = Command::new(&uj)
        .arg("--precise")
        .arg("true")
        .arg("--format")
        .arg("json")
        .arg("contest")
        .arg(contest_dir)
        .output()
        .expect("failed to execute uj contest with precise");

    assert!(
        out.status.success(),
        "Contest with precise failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("Must be valid JSON");

    // Check reports field in JSON output
    let reports = json_val["reports"]
        .as_array()
        .expect("reports array expected in JSON");
    let mut rejudged_found = false;
    for rep in reports {
        if let Some(tests) = rep["tests"].as_array() {
            for t in tests {
                if t["timing"]["rejudged"].as_bool() == Some(true) {
                    rejudged_found = true;
                    break;
                }
            }
        }
        if rejudged_found {
            break;
        }
    }

    let html_content = fs::read_to_string(contest_dir.join("contest_report.html")).unwrap();
    assert!(
        rejudged_found || html_content.contains("Rejudged"),
        "Precise rejudging must be executed for borderline edge cases"
    );
}

#[test]
fn contest_ndjson_stream() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let contest_dir = temp.path();

    let tail_fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contests/tail");
    if !tail_fixture.exists() {
        return;
    }
    copy_dir_all(&tail_fixture, contest_dir).unwrap();

    let out = Command::new(&uj)
        .arg("--format")
        .arg("ndjson")
        .arg("contest")
        .arg(contest_dir)
        .output()
        .expect("failed to execute uj contest with ndjson");

    assert!(
        out.status.success(),
        "Contest with ndjson failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(!lines.is_empty(), "Ndjson output must not be empty");

    let mut event_count = 0;
    for (idx, line) in lines.iter().enumerate() {
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Line {idx} is not valid JSON: '{line}': {e}"));
        if idx == lines.len() - 1 {
            assert_eq!(val["event"], "contest_finished");
            assert_eq!(val["ok"], true);
            assert!(val["standings"].is_array());
        } else {
            assert!(
                val.get("submission").is_some(),
                "PoolEvent must contain submission ref: {line}"
            );
            assert!(
                val.get("event").is_some(),
                "PoolEvent must contain event name: {line}"
            );
            event_count += 1;
        }
    }
    assert!(
        event_count > 0,
        "Must have streamed pool events before contest_finished"
    );
}

#[test]
fn test_contest_fail_fast_skips() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let root = temp.path();

    // 1. Copy tests/contests/tail to tempdir
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tail_dir = manifest_dir.join("../../tests/contests/tail");
    let contest_dir = root.join("tail");
    copy_dir_all(&tail_dir, &contest_dir).unwrap();

    // 2. Append subtask min scoring to problems/slowsum/problem.toml in the copy
    let p_toml = contest_dir.join("problems/slowsum/problem.toml");
    let subtask_cfg = "\n[[subtasks]]\nid = 1\nscore = 100.0\nscoring = \"min\"\ntests = [\"*\"]\n";
    let mut cur = fs::read_to_string(&p_toml).unwrap();
    cur.push_str(subtask_cfg);
    fs::write(&p_toml, cur).unwrap();

    // 3. Execute uj --fail-fast --jobs 1 --format json contest <dir>
    let out = Command::new(&uj)
        .arg("--fail-fast")
        .arg("--jobs")
        .arg("1")
        .arg("--format")
        .arg("json")
        .arg("contest")
        .arg(&contest_dir)
        .output()
        .expect("failed to execute uj contest");

    assert!(
        out.status.success(),
        "Contest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout_str)
        .unwrap_or_else(|e| panic!("Invalid JSON: {e}, stdout: {stdout_str}"));

    let reports = val["reports"].as_array().expect("expected reports array");
    let tle1_report = reports
        .iter()
        .find(|r| {
            r["submission"]
                .as_str()
                .map(|s| s.contains("tle1") && s.contains("slowsum"))
                .unwrap_or(false)
        })
        .expect("could not find tle1 - slowsum report");

    let tests = tle1_report["tests"]
        .as_array()
        .expect("expected tests array");
    assert_eq!(tests.len(), 20);

    let skipped_count = tests
        .iter()
        .filter(|t| t["verdict"].as_str() == Some("SKIPPED"))
        .count();
    let tle_count = tests
        .iter()
        .filter(|t| t["verdict"].as_str() == Some("TLE"))
        .count();

    assert_eq!(
        skipped_count, 19,
        "Expected 19 skipped tests, got {skipped_count}"
    );
    assert_eq!(tle_count, 1, "Expected 1 TLE test, got {tle_count}");
}

#[test]
fn test_contest_filter_problem_and_contestant() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp = tempdir().unwrap();
    let contest_dir = temp.path();

    let tail_fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contests/tail");
    if !tail_fixture.exists() {
        return;
    }
    copy_dir_all(&tail_fixture, contest_dir).unwrap();

    let out = Command::new(&uj)
        .arg("--format")
        .arg("json")
        .arg("contest")
        .arg(contest_dir)
        .arg("--problem")
        .arg("aplusb")
        .arg("--contestant")
        .arg("ac1,ce1")
        .output()
        .expect("failed to execute uj contest with filters");

    assert!(
        out.status.success(),
        "Contest with filters failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("Must be valid JSON");
    assert_eq!(val["ok"], true);
    assert_eq!(val["problems_count"], 1);
    assert_eq!(val["contestants_count"], 2);

    let standings = val["standings"].as_array().unwrap();
    assert_eq!(standings.len(), 2);
    assert_eq!(standings[0]["name"], "ac1");
    assert_eq!(standings[0]["total_score"], 100.0);
    assert_eq!(standings[1]["name"], "ce1");
    assert_eq!(standings[1]["total_score"], 0.0);
}

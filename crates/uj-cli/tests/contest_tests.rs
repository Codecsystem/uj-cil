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

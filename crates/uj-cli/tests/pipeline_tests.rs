use std::process::Command;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn get_uj_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // pop test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(if cfg!(windows) { "uj.exe" } else { "uj" })
}

fn get_workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn uj_cmd() -> Command {
    let mut cmd = Command::new(get_uj_bin());
    cmd.current_dir(get_workspace_root());
    cmd
}

#[test]
fn test_judge_pipeline_ac() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Expected exit code 0 for AC solution. Stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout_str}"));

    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
    assert_eq!(report["tests"].as_array().unwrap().len(), 3);
    assert_eq!(report["subtasks"].as_array().unwrap().len(), 2);
}

#[test]
fn test_judge_pipeline_wa() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/wa.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1), "Expected exit code 1 for WA");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    assert_eq!(report["verdict"], "WA");
    assert_eq!(report["score"], 0.0);

    // Verify diff summary is present
    let test1 = &report["tests"][0];
    assert_eq!(test1["verdict"], "WA");
    assert!(test1["diff"]["line"].is_number());
    assert_eq!(test1["diff"]["expected"], "3");
    assert_eq!(test1["diff"]["found"], "4");
}

#[test]
fn test_judge_pipeline_tle() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/tle.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(1),
        "Expected exit code 1 for TLE"
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    assert_eq!(report["verdict"], "TLE");
    assert_eq!(report["score"], 0.0);
}

#[test]
fn test_judge_pipeline_re() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/re.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1), "Expected exit code 1 for RE");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    assert_eq!(report["verdict"], "RE");
}

#[test]
fn test_judge_pipeline_ce() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ce.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(5), "Expected exit code 5 for CE");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    assert_eq!(report["verdict"], "CE");
    assert!(report["compile_error"].is_string());
}

#[test]
fn test_last_and_show() {
    let _guard = lock_test();

    // 1. Run AC
    let _ = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .output()
        .unwrap();

    // 2. Query last
    let last_output = uj_cmd()
        .arg("last")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_eq!(last_output.status.code(), Some(0));
    let last_report: serde_json::Value = serde_json::from_slice(&last_output.stdout).unwrap();
    assert_eq!(last_report["verdict"], "AC");

    // 3. Query show --test 1
    let show_output = uj_cmd()
        .arg("show")
        .arg("--test")
        .arg("1")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_eq!(show_output.status.code(), Some(0));
    let show_test: serde_json::Value = serde_json::from_slice(&show_output.stdout).unwrap();
    assert_eq!(show_test["id"], "1");
    assert_eq!(show_test["verdict"], "AC");
}

#[test]
fn test_parallel_consistency() {
    let _guard = lock_test();

    // Run with 1 slot
    let out_j1 = uj_cmd()
        .arg("-j")
        .arg("1")
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_eq!(out_j1.status.code(), Some(0));
    let rep_j1: serde_json::Value = serde_json::from_slice(&out_j1.stdout).unwrap();

    // Run with 4 slots
    let out_j4 = uj_cmd()
        .arg("-j")
        .arg("4")
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_eq!(out_j4.status.code(), Some(0));
    let rep_j4: serde_json::Value = serde_json::from_slice(&out_j4.stdout).unwrap();

    assert_eq!(rep_j1["verdict"], rep_j4["verdict"]);
    assert_eq!(rep_j1["score"], rep_j4["score"]);
    assert_eq!(
        rep_j1["tests"].as_array().unwrap().len(),
        rep_j4["tests"].as_array().unwrap().len()
    );
}

#[test]
fn test_judge_pipeline_testlib_checker() {
    let _guard = lock_test();

    // 1. Test AC on testlib problem
    let ac_out = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/testlib_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");
    assert_eq!(ac_out.status.code(), Some(0));
    let ac_rep: serde_json::Value = serde_json::from_slice(&ac_out.stdout).unwrap();
    assert_eq!(ac_rep["verdict"], "AC");
    assert_eq!(ac_rep["score"], 100.0);
    assert!(
        ac_rep["tests"][0]["checker"]
            .as_str()
            .unwrap()
            .contains("correct sum")
    );

    // 2. Test WA on testlib problem
    let wa_out = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/wa.cpp")
        .arg("-p")
        .arg("tests/problems/testlib_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");
    assert_eq!(wa_out.status.code(), Some(1));
    let wa_rep: serde_json::Value = serde_json::from_slice(&wa_out.stdout).unwrap();
    assert_eq!(wa_rep["verdict"], "WA");
    assert_eq!(wa_rep["score"], 0.0);
    assert!(
        wa_rep["tests"][0]["checker"]
            .as_str()
            .unwrap()
            .contains("expected")
    );
}

#[test]
fn test_judge_pipeline_lemon_checker() {
    let _guard = lock_test();

    // 1. Test AC on Lemon SPJ problem
    let ac_out = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/lemon_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");
    assert_eq!(ac_out.status.code(), Some(0));
    let ac_rep: serde_json::Value = serde_json::from_slice(&ac_out.stdout).unwrap();
    assert_eq!(ac_rep["verdict"], "AC");
    assert_eq!(ac_rep["score"], 100.0);
    assert_eq!(ac_rep["tests"][0]["checker"], "Perfect match");

    // 2. Test Partial Score on Lemon SPJ problem
    let pc_out = uj_cmd()
        .arg("judge")
        .arg("tests/problems/lemon_problem/solutions/partial.cpp")
        .arg("-p")
        .arg("tests/problems/lemon_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");
    assert_eq!(pc_out.status.code(), Some(1));
    let pc_rep: serde_json::Value = serde_json::from_slice(&pc_out.stdout).unwrap();
    assert_eq!(pc_rep["verdict"], "PC");
    assert_eq!(pc_rep["score"], 40.0);
    assert!(
        pc_rep["tests"][0]["checker"]
            .as_str()
            .unwrap()
            .contains("Close answer")
    );
}

#[test]
fn test_judge_pipeline_fail_fast() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/wa.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--fail-fast")
        .arg("-j")
        .arg("1")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    // Verify subtask dependency skipping or fail-fast behavior
    assert_eq!(report["verdict"], "WA");
    let tests = report["tests"].as_array().unwrap();
    // At least one test in subtask 1 was evaluated as WA
    assert!(tests.iter().any(|t| t["verdict"] == "WA"));
}

#[test]
fn test_judge_pipeline_interactive_ac() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/interactive_problem/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/interactive_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Interactive AC solution failed. Stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
}

#[test]
fn test_judge_pipeline_interactive_wa() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/interactive_problem/solutions/wa.cpp")
        .arg("-p")
        .arg("tests/problems/interactive_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "WA");
    assert_eq!(report["score"], 0.0);
}

#[test]
fn test_judge_pipeline_interactive_deadlock() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/interactive_problem/solutions/deadlock.cpp")
        .arg("-p")
        .arg("tests/problems/interactive_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "TLE");
    assert_eq!(report["score"], 0.0);
    let msg = report["tests"][0]["checker"].as_str().unwrap_or_default();
    assert!(msg.contains("Idleness limit exceeded") || msg.contains("Process idleness"));
}

#[test]
fn test_judge_pipeline_grader_ac() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/grader_problem/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/grader_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Grader AC solution failed. Stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
}

#[test]
fn test_judge_pipeline_grader_wa() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/grader_problem/solutions/wa.cpp")
        .arg("-p")
        .arg("tests/problems/grader_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "WA");
    assert_eq!(report["score"], 0.0);
}

#[test]
fn test_judge_pipeline_output_only_ac() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/output_only_problem/solutions/ac")
        .arg("-p")
        .arg("tests/problems/output_only_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Output-only AC failed. Stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
    assert_eq!(report["tests"].as_array().unwrap().len(), 2);
}

#[test]
fn test_judge_pipeline_output_only_wa() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/output_only_problem/solutions/wa")
        .arg("-p")
        .arg("tests/problems/output_only_problem")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(output.status.code(), Some(1));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "PC");
    assert_eq!(report["score"], 50.0);
}

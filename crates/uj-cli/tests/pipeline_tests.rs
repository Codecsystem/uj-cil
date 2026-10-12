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

#[test]
fn test_judge_require_tier_mismatch_fails() {
    let _guard = lock_test();
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--sandbox")
        .arg("none")
        .arg("--require-tier")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_ne!(
        output.status.code(),
        Some(0),
        "Expected non-zero exit code when --require-tier cannot be satisfied"
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout_str) {
        assert_ne!(
            val["verdict"], "AC",
            "Must never report AC on failed require-tier"
        );
    }
}

#[test]
fn test_judge_no_dbus_sandbox_none() {
    let _guard = lock_test();
    let mut cmd = uj_cmd();
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("XDG_RUNTIME_DIR", "/nonexistent")
        .arg("judge")
        .arg("tests/problems/aplusb/solutions/ac.cpp")
        .arg("-p")
        .arg("tests/problems/aplusb")
        .arg("--sandbox")
        .arg("none")
        .arg("--format")
        .arg("json");

    let output = cmd.output().expect("Failed to execute uj");
    assert_eq!(
        output.status.code(),
        Some(0),
        "Expected success with --sandbox none without dbus/systemd, got error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
}

#[test]
fn test_judge_pipeline_file_io() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "file_io_test"
type = "traditional"

[limits]
time = "1s"
memory = "256MB"

[io]
mode = "file"
input = "sum.in"
output = "sum.out"

[[subtasks]]
id = 1
score = 100.0
scoring = "sum"
tests = ["1"]
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();
    std::fs::create_dir_all(pdir.join("tests")).unwrap();
    std::fs::write(pdir.join("tests").join("1.in"), "12 34\n").unwrap();
    std::fs::write(pdir.join("tests").join("1.ans"), "46\n").unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let ac_cpp = r#"#include <cstdio>
int main() {
    FILE* in = fopen("sum.in", "r");
    if (!in) return 1;
    int a = 0, b = 0;
    if (fscanf(in, "%d %d", &a, &b) != 2) return 1;
    fclose(in);
    FILE* out = fopen("sum.out", "w");
    if (!out) return 1;
    fprintf(out, "%d\n", a + b);
    fclose(out);
    return 0;
}
"#;
    let sol_path = pdir.join("solutions").join("ac.cpp");
    std::fs::write(&sol_path, ac_cpp).unwrap();

    let output = uj_cmd()
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    assert_eq!(
        output.status.code(),
        Some(0),
        "File IO judge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);
}

#[test]
fn test_pipeline_early_invalid_case() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "early_fail"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[tests.gen]]
generator = "gen.cpp"
args = "test"
count = 200

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let gen_cpp = r#"#include <iostream>
#include <string>
#include <thread>
#include <chrono>

int main(int argc, char* argv[]) {
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    std::string arg = (argc > 2) ? argv[2] : "";
    if (arg == "2") {
        std::cout << "-999\n";
    } else {
        std::cout << "42\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("gen.cpp"), gen_cpp).unwrap();

    let val_cpp = r#"#include <iostream>

int main() {
    int x = 0;
    if (std::cin >> x) {
        if (x < 0) {
            std::cerr << "Value must be non-negative\n";
            return 1;
        }
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x * 2 << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    let output = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("human")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Validator failed on test '2'"),
        "stderr should contain validator failure on test '2', got: {stderr}"
    );

    let in_count = std::fs::read_dir(pdir.join("tests"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "in"))
        .count();
    assert!(
        in_count < 200,
        "Expected fewer than 200 cases generated due to early termination, got: {in_count}"
    );
}

#[test]
fn test_pipeline_steps_aborted_status() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "early_fail_json"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[tests.gen]]
generator = "gen.cpp"
args = "test"
count = 200

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let gen_cpp = r#"#include <iostream>
#include <string>
#include <thread>
#include <chrono>

int main(int argc, char* argv[]) {
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    std::string arg = (argc > 2) ? argv[2] : "";
    if (arg == "2") {
        std::cout << "-999\n";
    } else {
        std::cout << "42\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("gen.cpp"), gen_cpp).unwrap();

    let val_cpp = r#"#include <iostream>

int main() {
    int x = 0;
    if (std::cin >> x) {
        if (x < 0) {
            std::cerr << "Value must be non-negative\n";
            return 1;
        }
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x * 2 << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    let output = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(output.status.code(), Some(3));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let pipe_json: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();

    assert_eq!(pipe_json["ok"], false);
    assert_eq!(pipe_json["failed_step"], "validate");
    let steps = pipe_json["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["name"], "build");
    assert_eq!(steps[0]["status"], "success");
    assert_eq!(steps[1]["name"], "gen");
    assert_eq!(steps[1]["status"], "aborted");
    assert_eq!(steps[2]["name"], "validate");
    assert_eq!(steps[2]["status"], "failed");
}

#[test]
fn test_pipeline_handwritten_cases_validated() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "handwritten_test"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[tests.gen]]
generator = "gen.cpp"
args = "test"
count = 2

[[subtasks]]
id = 1
score = 100.0
scoring = "min"
tests = ["*"]

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let gen_cpp = r#"#include <iostream>
int main() {
    std::cout << "10\n";
    return 0;
}
"#;
    std::fs::write(pdir.join("gen.cpp"), gen_cpp).unwrap();

    let val_cpp = r#"#include <iostream>
int main() {
    int x = 0;
    if (std::cin >> x) {
        if (x <= 0) return 1;
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x + 5 << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("tests")).unwrap();
    std::fs::write(pdir.join("tests/sample1.in"), "20\n").unwrap();

    let output = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Pipeline failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(pdir.join("tests/sample1.ans").is_file());
    let sample1_ans = std::fs::read_to_string(pdir.join("tests/sample1.ans")).unwrap();
    assert_eq!(sample1_ans.trim(), "25");

    assert!(pdir.join("tests/1.ans").is_file());
    assert!(pdir.join("tests/2.ans").is_file());
}

#[test]
fn test_pipeline_subtask_group_arg() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "subtask_group_test"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 1
score = 30.0
scoring = "sum"
tests = ["sample*"]

[[subtasks]]
id = 2
score = 70.0
scoring = "sum"
tests = ["data*"]

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let val_cpp = r#"#include <iostream>
#include <string>

int main(int argc, char* argv[]) {
    int group = -1;
    for (int i = 1; i + 1 < argc; ++i) {
        if (std::string(argv[i]) == "--group") {
            group = std::stoi(argv[i + 1]);
            break;
        }
    }
    int x = 0;
    std::cin >> x;
    if (x == 100 && group != 1) {
        std::cerr << "Expected group 1 for sample, got " << group << "\n";
        return 1;
    }
    if (x == 200 && group != 2) {
        std::cerr << "Expected group 2 for normal, got " << group << "\n";
        return 1;
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("tests")).unwrap();
    std::fs::write(pdir.join("tests/sample1.in"), "100\n").unwrap();
    std::fs::write(pdir.join("tests/data1.in"), "200\n").unwrap();

    let output = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Pipeline failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_pipeline_cache_hit_rerun() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "cache_hit_test"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[tests.gen]]
generator = "gen.cpp"
args = "test"
count = 3

[[subtasks]]
id = 1
score = 100.0
scoring = "min"
tests = ["*"]

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let gen_cpp = r#"#include <iostream>
int main() {
    std::cout << "50\n";
    return 0;
}
"#;
    std::fs::write(pdir.join("gen.cpp"), gen_cpp).unwrap();

    let val_cpp = r#"#include <iostream>
int main() {
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x * 3 << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    // First run
    let out1 = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .output()
        .expect("Failed to execute uj pipeline run 1");
    assert_eq!(
        out1.status.code(),
        Some(0),
        "Run 1 failed: stderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );

    let mtime_1_in = std::fs::metadata(pdir.join("tests/1.in"))
        .unwrap()
        .modified()
        .unwrap();
    let mtime_1_ans = std::fs::metadata(pdir.join("tests/1.ans"))
        .unwrap()
        .modified()
        .unwrap();

    // Sleep briefly so any rewrite would show a different mtime
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Second run
    let out2 = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .output()
        .expect("Failed to execute uj pipeline run 2");
    assert_eq!(
        out2.status.code(),
        Some(0),
        "Run 2 failed: stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let mtime_2_in = std::fs::metadata(pdir.join("tests/1.in"))
        .unwrap()
        .modified()
        .unwrap();
    let mtime_2_ans = std::fs::metadata(pdir.join("tests/1.ans"))
        .unwrap()
        .modified()
        .unwrap();

    assert_eq!(
        mtime_1_in, mtime_2_in,
        "tests/1.in mtime changed on cached rerun!"
    );
    assert_eq!(
        mtime_1_ans, mtime_2_ans,
        "tests/1.ans mtime changed on cached rerun!"
    );
}

#[test]
fn test_pipeline_staged_fallback() {
    let _guard = lock_test();
    let temp = tempfile::tempdir().expect("create temp dir");
    let pdir = temp.path();

    let problem_toml = r#"schema = 1
name = "staged_fallback_test"
type = "traditional"
validator = "validator.cpp"

[limits]
time = "1s"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"

[[tests.gen]]
generator = "gen.cpp"
args = "test"
count = 2

[[solutions]]
path = "solutions/std.cpp"
tag = "main"
"#;
    std::fs::write(pdir.join("problem.toml"), problem_toml).unwrap();

    let gen_cpp = r#"#include <iostream>
int main() {
    std::cout << "7\n";
    return 0;
}
"#;
    std::fs::write(pdir.join("gen.cpp"), gen_cpp).unwrap();

    let val_cpp = r#"#include <iostream>
int main() {
    return 0;
}
"#;
    std::fs::write(pdir.join("validator.cpp"), val_cpp).unwrap();

    std::fs::create_dir_all(pdir.join("solutions")).unwrap();
    let std_cpp = r#"#include <iostream>
int main() {
    int x;
    if (std::cin >> x) {
        std::cout << x * 10 << "\n";
    }
    return 0;
}
"#;
    std::fs::write(pdir.join("solutions/std.cpp"), std_cpp).unwrap();

    let output = uj_cmd()
        .env("UJ_PIPELINE_STAGED", "1")
        .arg("pipeline")
        .arg("-p")
        .arg(pdir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(
        output.status.code(),
        Some(0),
        "Staged pipeline failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let pipe_json: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(pipe_json["ok"], true);
    assert_eq!(pipe_json["steps"].as_array().map(|a| a.len()), Some(6));
}

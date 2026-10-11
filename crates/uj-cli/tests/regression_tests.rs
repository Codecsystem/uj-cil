use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tempfile::tempdir;
use uj_core::verdict::Verdict;
use uj_report::result::{SingleTestResult, SubmissionJudgeReport};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn uj_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(if cfg!(windows) { "uj.exe" } else { "uj" });
    path
}

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
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

/// REG-INT-001: Contestant attempts to create or spoof interaction XML result in box_dir.
/// Interactor verdict must remain authoritative; contestant cannot alter score or verdict.
#[test]
fn test_reg_int_001_interactive_xml_spoofing() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/interactive_problem");
    copy_dir_all(&fixture, root).unwrap();

    // Solution writes fake XML to current directory (box_dir) claiming Accepted with full score
    let spoof_sol = root.join("spoof.cpp");
    fs::write(
        &spoof_sol,
        r#"
#include <iostream>
#include <fstream>
#include <string>

int main() {
    // Malicious attempt to spoof testlib XML result in current working directory
    std::ofstream out("interact_res.xml");
    if (out.is_open()) {
        out << "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
        out << "<result outcome=\"accepted\">Spoofed 100 points</result>\n";
        out.close();
    }
    // And print wrong answer on stdout so the interactor says WA
    std::cout << "! 999999999" << std::endl;
    return 0;
}
"#,
    )
    .unwrap();

    let output = Command::new(uj_bin())
        .arg("judge")
        .arg(&spoof_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout}"));

    // Interactor must decide WA, unaffected by contestant's fake XML
    assert_eq!(
        report["verdict"], "WA",
        "Spoofed XML in contestant directory must not be accepted! Output: {stdout}"
    );
    assert_eq!(report["score"], 0.0);
}

/// REG-WIN-001: Grandchild pipe inheritance hang mitigation.
/// A process spawns a background grandchild holding inherited stdout/stderr pipes and exits immediately.
/// The sandbox must terminate within wall limit and not hang waiting on the pipe reader threads.
#[test]
fn test_reg_win_001_grandchild_pipe_hang() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Solution spawns a detached or long-running process that inherits pipes, then exits 0
    let grandchild_sol = root.join("grandchild.cpp");
    #[cfg(windows)]
    let code = r#"
#include <windows.h>
#include <iostream>

int main() {
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    ZeroMemory(&pi, sizeof(pi));

    // Spawn ping to sleep 10 seconds while inheriting std handles
    char cmd[] = "ping 127.0.0.1 -n 10";
    CreateProcessA(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi);
    // Parent exits immediately
    return 0;
}
"#;
    #[cfg(not(windows))]
    let code = r#"
#include <unistd.h>
#include <stdlib.h>

int main() {
    if (fork() == 0) {
        // Child sleeps while holding open stdout/stderr
        sleep(10);
        _exit(0);
    }
    // Parent exits immediately
    return 0;
}
"#;
    fs::write(&grandchild_sol, code).unwrap();

    let start = std::time::Instant::now();
    let output = Command::new(uj_bin())
        .arg("judge")
        .arg(&grandchild_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let elapsed = start.elapsed();
    // Wall limit is 2-3s; it must terminate well before 10s
    assert!(
        elapsed.as_secs() < 8,
        "Grandchild pipe hold caused engine to hang! Elapsed: {:?}",
        elapsed
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let _report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout}"));
}

/// REG-ART-001: Missing answer files or unreadable answer files produce JudgeFailed,
/// not false Accepted. Legitimate 0-byte answer file is handled properly.
#[test]
fn test_reg_art_001_missing_answer_yields_judge_failed() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Delete answer file for test 1 (in tests/1.ans)
    let ans1 = root.join("tests/1.ans");
    let _ = fs::remove_file(&ans1);

    let sol = root.join("solutions/ac.cpp");
    let output = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout}"));

    // Missing answer file MUST yield SE (JudgeFailed), not AC
    assert_eq!(
        report["verdict"], "SE",
        "Missing answer file must cause SE/JudgeFailed, got: {stdout}"
    );

    let test1 = &report["tests"][0];
    assert_eq!(test1["verdict"], "SE");
    let msg = test1["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("Answer file not found") || msg.contains("No answer file specified"),
        "Expected error message indicating missing answer file, got: {:?}",
        test1["message"]
    );
}

/// REG-ART-002: File I/O output missing yields WA (no stdout fallback);
/// output file exceeding limit yields OLE.
#[test]
fn test_reg_art_002_file_io_missing_output_and_output_limit() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Reconfigure problem to use file I/O with 64 bytes output limit
    let problem_toml = root.join("problem.toml");
    let toml_content = r#"schema = 1
name = "aplusb"
type = "traditional"

[limits]
time = "2s"
wall_time = "10s"
memory = "512MiB"
output = "64B"

[io]
mode = "file"
input_name = "aplusb.in"
output_name = "aplusb.out"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 1
score = 100.0
scoring = "min"
tests = ["1"]
"#;
    fs::write(&problem_toml, toml_content).unwrap();

    // 1. Solution writes to stdout instead of aplusb.out
    let stdout_only_sol = root.join("stdout_only.cpp");
    fs::write(
        &stdout_only_sol,
        r#"
#include <iostream>
int main() {
    // Only prints to stdout; does NOT create aplusb.out
    std::cout << 3 << std::endl;
    return 0;
}
"#,
    )
    .unwrap();

    let out1 = Command::new(uj_bin())
        .arg("judge")
        .arg(&stdout_only_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let report1: serde_json::Value = serde_json::from_str(&stdout1)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout1}"));

    // Must be WA because required output file was not created (no silent stdout fallback!)
    assert_eq!(
        report1["verdict"], "WA",
        "Missing required output file in file I/O must yield WA! Report: {stdout1}"
    );

    // 2. Solution creates aplusb.out with 1024 bytes (limit is 64 B)
    let ole_sol = root.join("ole.cpp");
    fs::write(
        &ole_sol,
        r#"
#include <fstream>
#include <string>

int main() {
    std::ofstream out("aplusb.out");
    // Write 1024 bytes
    std::string big(1024, 'x');
    out << big;
    return 0;
}
"#,
    )
    .unwrap();

    let out2 = Command::new(uj_bin())
        .arg("judge")
        .arg(&ole_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let report2: serde_json::Value = serde_json::from_str(&stdout2)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout2}"));

    assert_eq!(
        report2["verdict"], "OLE",
        "Output file exceeding limit must yield OLE! Report: {stdout2}"
    );
}

/// REG-SCORE-001: Subtask DAG order invariance.
/// Reordering subtask declarations in problem.toml must NOT change score or evaluation results.
#[test]
fn test_reg_score_001_subtask_dag_order_invariance() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Order A: Subtask 1 defined before Subtask 2. Subtask 2 depends on 1.
    let toml_order_a = r#"schema = 1
name = "aplusb"
type = "traditional"

[limits]
time = "1s"
memory = "512MiB"

[io]
mode = "stdio"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 1
score = 40.0
scoring = "min"
tests = ["1"]

[[subtasks]]
id = 2
score = 60.0
scoring = "min"
depends = [1]
tests = ["2"]
"#;

    // Order B: Subtask 2 declared before Subtask 1, but still depends on 1.
    let toml_order_b = r#"schema = 1
name = "aplusb"
type = "traditional"

[limits]
time = "1s"
memory = "512MiB"

[io]
mode = "stdio"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 2
score = 60.0
scoring = "min"
depends = [1]
tests = ["2"]

[[subtasks]]
id = 1
score = 40.0
scoring = "min"
tests = ["1"]
"#;

    // Solution that fails test 1 (gives WA on 1) but is correct for test 2
    let partial_sol = root.join("partial.cpp");
    fs::write(
        &partial_sol,
        r#"
#include <iostream>
int main() {
    int a, b;
    if (std::cin >> a >> b) {
        if (a == 1 && b == 2) {
            std::cout << 999 << std::endl; // Wrong answer for test 1
        } else {
            std::cout << (a + b) << std::endl; // Correct for other tests
        }
    }
    return 0;
}
"#,
    )
    .unwrap();

    // Test Order A
    fs::write(root.join("problem.toml"), toml_order_a).unwrap();
    let out_a = Command::new(uj_bin())
        .arg("judge")
        .arg(&partial_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep_a: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out_a.stdout)).unwrap();

    // Test Order B
    fs::write(root.join("problem.toml"), toml_order_b).unwrap();
    let out_b = Command::new(uj_bin())
        .arg("judge")
        .arg(&partial_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep_b: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out_b.stdout)).unwrap();

    // Scores must be identical regardless of declaration order!
    assert_eq!(rep_a["score"], rep_b["score"]);
    assert_eq!(rep_a["verdict"], rep_b["verdict"]);
    // Since subtask 2 depends on subtask 1 and subtask 1 failed, subtask 2 must not be awarded points!
    assert_eq!(rep_a["score"], 0.0);
    assert_eq!(rep_b["score"], 0.0);
}

/// REG-SCORE-002: Error priority over partial credit.
/// Subtask 1 has AC (50 pts), Subtask 2 has SE/JudgeFailed or TLE (0 pts).
/// The overall verdict must preserve SE/TLE and NOT be masked into PC.
#[test]
fn test_reg_score_002_system_error_priority_over_pc() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Subtask 1 has test 1 (50 pts), Subtask 2 has test 2 (50 pts)
    let toml_content = r#"schema = 1
name = "aplusb"
type = "traditional"

[limits]
time = "1s"
memory = "512MiB"

[io]
mode = "stdio"

[checker]
type = "builtin"
name = "wcmp"

[[subtasks]]
id = 1
score = 50.0
scoring = "min"
tests = ["1"]

[[subtasks]]
id = 2
score = 50.0
scoring = "min"
tests = ["2"]
"#;
    fs::write(root.join("problem.toml"), toml_content).unwrap();

    // Corrupt test 2's answer file so test 2 results in JudgeFailed (SE)
    let _ = fs::remove_file(root.join("tests/2.ans"));

    let sol = root.join("solutions/ac.cpp");
    let output = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout}"));

    // Overall verdict MUST preserve SE, not be downgraded/masked to PC
    assert_eq!(
        report["verdict"], "SE",
        "System error must NOT be masked into PC! Report: {stdout}"
    );
}

/// REG-CACHE-001: Modification to included headers forces recompilation and never reuses stale binary.
#[test]
fn test_reg_cache_001_header_modification_recompiles() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Create a local header file config.h
    let header_path = root.join("config.h");
    fs::write(&header_path, "#define OFFSET 0\n").unwrap();

    let sol = root.join("offset_sol.cpp");
    fs::write(
        &sol,
        r#"
#include <iostream>
#include "config.h"

int main() {
    int a, b;
    if (std::cin >> a >> b) {
        std::cout << (a + b + OFFSET) << std::endl;
    }
    return 0;
}
"#,
    )
    .unwrap();

    // First run with OFFSET 0 -> Accepted
    let out1 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let rep1: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out1.stdout)).unwrap();
    assert_eq!(rep1["verdict"], "AC");

    // Modify header to OFFSET 100 -> Must recompile and yield WA
    fs::write(&header_path, "#define OFFSET 100\n").unwrap();

    let out2 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let rep2: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out2.stdout)).unwrap();
    assert_eq!(
        rep2["verdict"], "WA",
        "Header change was not detected! Stale binary was incorrectly reused."
    );
}

/// REG-CACHE-002: Corrupted or truncated binary cache recovery.
/// If cached binary is truncated to 0 bytes, the compiler detects corruption and rebuilds.
#[test]
fn test_reg_cache_002_corrupted_binary_recovery() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    let sol = root.join("solutions/ac.cpp");

    // 1. Initial judge run builds and caches binary
    let out1 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep1: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out1.stdout)).unwrap();
    assert_eq!(rep1["verdict"], "AC");

    // 2. Truncate any binary in .uj/tmp/bin to 0 bytes
    let tmp_bin = root.join(".uj/tmp/bin");
    if tmp_bin.exists() {
        for entry in fs::read_dir(&tmp_bin).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                let _ = fs::write(entry.path(), b"");
            }
        }
    }

    // 3. Second judge run must detect truncated binary, recover cleanly, and still succeed (AC)
    let out2 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep2: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out2.stdout)).unwrap();
    assert_eq!(
        rep2["verdict"], "AC",
        "Failed to recover from corrupted/truncated binary cache! Report: {:?}",
        rep2
    );
}

/// REG-CACHE-003: Transitive and nested header modifications trigger recompilation.
/// Solution includes a subfolder header which includes a nested header.
/// Editing the deeply nested header must invalidate compilation cache.
#[test]
fn test_reg_cache_003_nested_header_recompiles() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    let sub_dir = root.join("sub");
    let inner_dir = sub_dir.join("inner");
    fs::create_dir_all(&inner_dir).unwrap();

    let inner_header = inner_dir.join("inner.hpp");
    fs::write(&inner_header, "#pragma once\n#define BONUS 0\n").unwrap();

    let outer_header = sub_dir.join("outer.hpp");
    fs::write(
        &outer_header,
        "#pragma once\n#include \"inner/inner.hpp\"\n#define VAL BONUS\n",
    )
    .unwrap();

    let sol = root.join("nested_sol.cpp");
    fs::write(
        &sol,
        r#"
#include <iostream>
#include "sub/outer.hpp"

int main() {
    int a, b;
    if (std::cin >> a >> b) {
        std::cout << (a + b + VAL) << std::endl;
    }
    return 0;
}
"#,
    )
    .unwrap();

    // 1. Initial run with BONUS 0 -> AC
    let out1 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep1: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out1.stdout)).unwrap();
    assert_eq!(rep1["verdict"], "AC");

    // 2. Modify deeply nested header -> BONUS 999 -> Recompile and WA
    fs::write(&inner_header, "#pragma once\n#define BONUS 999\n").unwrap();

    let out2 = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");
    let rep2: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out2.stdout)).unwrap();
    assert_eq!(
        rep2["verdict"], "WA",
        "Nested header modification was not detected! Stale binary was incorrectly reused."
    );
}

/// REG-CHECK-001: UTF-8 multi-byte long tokens and NaN/Infinity checker scores.
/// Solution outputs multibyte UTF-8 characters (e.g. Chinese characters) to trigger WA diff snippet.
/// Slicing must not panic on byte boundary, and the judge report must be valid JSON with WA.
#[test]
fn test_reg_check_001_multibyte_token_and_nan_score() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // Solution prints a long UTF-8 multibyte string (50 Chinese characters = 150 bytes)
    let utf8_sol = root.join("utf8_sol.cpp");
    fs::write(
        &utf8_sol,
        r#"
#include <iostream>
int main() {
    for (int i = 0; i < 50; i++) {
        std::cout << "测试";
    }
    std::cout << std::endl;
    return 0;
}
"#,
    )
    .unwrap();

    let output = Command::new(uj_bin())
        .arg("judge")
        .arg(&utf8_sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("Failed to parse JSON report: {stdout}"));

    assert_eq!(report["verdict"], "WA");
    let test1 = &report["tests"][0];
    assert_eq!(test1["verdict"], "WA");
    assert!(!test1["diff"].is_null());
}

/// REG-AUTH-001: Authoring pipeline: init -> gen twice -> validate -> ans -> pipeline JSON.
/// Problem template is valid, repeated gen is idempotent (does not append duplicate tests),
/// and pipeline --format json emits a single valid root JSON object.
#[test]
fn test_reg_auth_001_authoring_pipeline_idempotence_and_json() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    // 1. uj init
    let init_out = Command::new(uj_bin())
        .arg("init")
        .arg(root)
        .output()
        .expect("failed to execute uj init");
    assert!(init_out.status.success(), "uj init failed: {:?}", init_out);

    // Verify problem.toml has validator parsed as top-level field
    let p_toml = fs::read_to_string(root.join("problem.toml")).unwrap();
    let cfg =
        uj_core::config::ProblemConfig::from_toml_str(&p_toml, Some(&root.join("problem.toml")))
            .expect("problem.toml generated by uj init should be valid");
    assert_eq!(
        cfg.validator.as_deref(),
        Some(Path::new("validator.cpp")),
        "Validator should be recognized at root of problem.toml"
    );

    // 2. uj gen (first run)
    let gen_out1 = Command::new(uj_bin())
        .arg("gen")
        .arg("-p")
        .arg(root)
        .output()
        .expect("failed to execute uj gen 1");
    assert!(gen_out1.status.success(), "uj gen 1 failed: {:?}", gen_out1);

    // Count .in files in tests/
    let count_tests = |dir: &Path| -> usize {
        fs::read_dir(dir.join("tests"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "in"))
            .count()
    };
    let initial_count = count_tests(root);
    assert!(
        initial_count >= 3,
        "Expected at least 3 tests, got {initial_count}"
    );

    // 3. uj gen (second run without --append: must be idempotent!)
    let gen_out2 = Command::new(uj_bin())
        .arg("gen")
        .arg("-p")
        .arg(root)
        .output()
        .expect("failed to execute uj gen 2");
    assert!(gen_out2.status.success(), "uj gen 2 failed: {:?}", gen_out2);
    let second_count = count_tests(root);
    assert_eq!(
        initial_count, second_count,
        "Subsequent uj gen without --append must be idempotent and not append tests!"
    );

    // 4. uj validate
    let val_out = Command::new(uj_bin())
        .arg("validate")
        .arg("-p")
        .arg(root)
        .output()
        .expect("failed to execute uj validate");
    assert!(
        val_out.status.success(),
        "uj validate failed: {:?}",
        val_out
    );

    // 5. uj ans
    let ans_out = Command::new(uj_bin())
        .arg("ans")
        .arg("-p")
        .arg(root)
        .output()
        .expect("failed to execute uj ans");
    assert!(ans_out.status.success(), "uj ans failed: {:?}", ans_out);

    // 6. uj pipeline --format json
    let pipe_out = Command::new(uj_bin())
        .arg("pipeline")
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj pipeline");
    assert!(
        pipe_out.status.success(),
        "uj pipeline failed: {:?}",
        pipe_out
    );

    let stdout_str = String::from_utf8_lossy(&pipe_out.stdout);
    let pipe_json: serde_json::Value = serde_json::from_str(&stdout_str).unwrap_or_else(|e| {
        panic!("uj pipeline stdout must be a single parseable JSON object! Err: {e}\nStdout: {stdout_str}")
    });
    assert_eq!(pipe_json["ok"], true);
    assert_eq!(pipe_json["steps"].as_array().map(|a| a.len()), Some(6));
}

/// REG-STRESS-001: Stress testing failure diagnostics.
/// When generator, target solution, or brute solution exits non-zero or crashes,
/// stress testing must return failure (ok=false), not ok=true, and report diagnostics.
#[test]
fn test_reg_stress_001_failure_diagnostics() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    // 1. All three programs fail (exit code 7)
    let bad_gen = root.join("bad_gen.cpp");
    fs::write(&bad_gen, "int main() { return 7; }\n").unwrap();
    let bad_sol = root.join("bad_sol.cpp");
    fs::write(&bad_sol, "int main() { return 7; }\n").unwrap();
    let bad_brute = root.join("bad_brute.cpp");
    fs::write(&bad_brute, "int main() { return 7; }\n").unwrap();

    let out1 = Command::new(uj_bin())
        .arg("stress")
        .arg("-p")
        .arg(root)
        .arg("--gen")
        .arg("bad_gen.cpp")
        .arg("--sol")
        .arg("bad_sol.cpp")
        .arg("--brute")
        .arg("bad_brute.cpp")
        .arg("-n")
        .arg("2")
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj stress");

    assert!(
        !out1.status.success(),
        "Stress test with crashing programs must fail"
    );
    let rep1: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out1.stdout))
        .unwrap_or_else(|_| {
            panic!(
                "Expected valid JSON error from stress: {}",
                String::from_utf8_lossy(&out1.stdout)
            )
        });
    assert_eq!(
        rep1["ok"], false,
        "Crashing generator must yield ok=false in stress report"
    );

    // 2. Generator succeeds, but target solution crashes (abort) while brute succeeds
    let good_gen = root.join("good_gen.cpp");
    fs::write(
        &good_gen,
        "#include <iostream>\nint main() { std::cout << \"1 2\\n\"; return 0; }\n",
    )
    .unwrap();
    let crashing_sol = root.join("crashing_sol.cpp");
    fs::write(
        &crashing_sol,
        "#include <cstdlib>\nint main() { std::abort(); }\n",
    )
    .unwrap();
    let good_brute = root.join("good_brute.cpp");
    fs::write(
        &good_brute,
        "#include <iostream>\nint main() { std::cout << 3 << std::endl; return 0; }\n",
    )
    .unwrap();

    let out2 = Command::new(uj_bin())
        .arg("stress")
        .arg("-p")
        .arg(root)
        .arg("--gen")
        .arg("good_gen.cpp")
        .arg("--sol")
        .arg("crashing_sol.cpp")
        .arg("--brute")
        .arg("good_brute.cpp")
        .arg("-n")
        .arg("1")
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj stress");

    assert!(
        !out2.status.success(),
        "Stress test with crashing candidate solution must fail"
    );
    let rep2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out2.stdout))
        .unwrap_or_else(|_| {
            panic!(
                "Expected valid JSON error from stress: {}",
                String::from_utf8_lossy(&out2.stdout)
            )
        });
    assert_eq!(
        rep2["ok"], false,
        "Crashing solution must yield ok=false in stress report"
    );
    // Verify that failing test case input was saved
    assert!(root.join("stress").exists());
}

/// REG-COMM-001: Multi-participant communication problem with isolated scoring
/// and sandbox tier preservation.
#[test]
fn test_reg_comm_001_multi_process_communication_and_tier_preservation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let problem_toml = r#"
schema = 1
name = "ring_comm"
title = "Ring Communication Problem"
type = "communication"
manager = "manager.cpp"
processes = 2
communication_topology = "ring"

[limits]
time = "3000ms"
memory = "256MB"

[[subtasks]]
id = 1
score = 100.0
tests = ["1"]
"#;
    fs::write(root.join("problem.toml"), problem_toml).unwrap();

    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();
    fs::write(tests_dir.join("1.in"), "7 8\n").unwrap();

    // Manager: reads 1.in, sends to participant 1 via stdout,
    // reads result from participant 2 via stdin, writes score to UJ_SCORE_FILE.
    let manager_cpp = r#"
#include <iostream>
#include <fstream>
#include <cstdlib>

int main(int argc, char* argv[]) {
    if (argc < 2) return 1;
    std::ifstream fin(argv[1]);
    int a = 0, b = 0;
    if (!(fin >> a >> b)) return 2;

    // Send to participant 1: stage 1, a, b
    std::cout << 1 << " " << a << " " << b << std::endl;

    // Receive final result from participant 2 via stdin
    int res = 0;
    if (!(std::cin >> res)) {
        return 3;
    }

    const char* score_path = std::getenv("UJ_SCORE_FILE");
    if (!score_path && argc >= 3) {
        score_path = argv[2];
    }
    if (score_path) {
        std::ofstream sf(score_path);
        if (res == (a + b) * 2) {
            sf << "1.0\n";
        } else {
            sf << "0.0\n";
        }
    }
    return 0;
}
"#;
    fs::write(root.join("manager.cpp"), manager_cpp).unwrap();

    // Solution:
    // If receives stage 1, adds a+b and passes "2 sum" to participant 2.
    // If receives stage 2, doubles the sum and outputs it to manager.
    let sol_cpp = r#"
#include <iostream>

int main() {
    int stage = 0;
    if (std::cin >> stage) {
        if (stage == 1) {
            int a = 0, b = 0;
            if (std::cin >> a >> b) {
                std::cout << 2 << " " << (a + b) << std::endl;
            }
        } else if (stage == 2) {
            int val = 0;
            if (std::cin >> val) {
                std::cout << (val * 2) << std::endl;
            }
        }
    }
    return 0;
}
"#;
    let sol_path = root.join("sol.cpp");
    fs::write(&sol_path, sol_cpp).unwrap();

    // Run uj judge
    let out = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj judge");

    assert!(
        out.status.success(),
        "Judge command must exit successfully: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rep: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .unwrap_or_else(|_| {
            panic!(
                "Expected valid JSON report: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });

    assert_eq!(
        rep["verdict"], "AC",
        "Correct multi-participant solution should yield AC"
    );
    assert_eq!(
        rep["score"], 100.0,
        "Correct solution should achieve full score"
    );

    // Verify sandbox tier is preserved (P1-01)
    let tier = rep["tests"][0]["sandbox_tier"].as_str().unwrap_or("");
    assert_ne!(
        tier, "degraded",
        "Contestant sandbox tier must not be degraded in communication problem, got: {}",
        tier
    );

    // Now test participant crash handling
    let bad_sol_cpp = r#"
#include <iostream>
#include <cstdlib>

int main() {
    int stage = 0;
    if (std::cin >> stage) {
        if (stage == 1) {
            std::cout << 2 << " " << 42 << std::endl;
        } else {
            std::abort();
        }
    }
    return 0;
}
"#;
    let bad_sol_path = root.join("bad_sol.cpp");
    fs::write(&bad_sol_path, bad_sol_cpp).unwrap();

    let bad_out = Command::new(uj_bin())
        .arg("judge")
        .arg(&bad_sol_path)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj judge for bad sol");

    let bad_rep: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&bad_out.stdout)).unwrap_or_else(|_| {
            panic!(
                "Expected valid JSON report: {}",
                String::from_utf8_lossy(&bad_out.stdout)
            )
        });

    let bad_verdict = bad_rep["verdict"].as_str().unwrap_or("");
    assert!(
        bad_verdict == "RE" || bad_verdict == "WA" || bad_verdict == "JudgeFailed",
        "Crashing participant must not be AC, got: {}",
        bad_verdict
    );
    assert_ne!(bad_rep["score"], 100.0);
}

/// REG-COMM-002: Multi-participant communication without explicit topology must be rejected
#[test]
fn test_reg_comm_002_multi_process_requires_explicit_topology() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let problem_toml = r#"
schema = 1
name = "unsupported_comm"
title = "Unsupported Comm Problem"
type = "communication"
manager = "manager.cpp"
processes = 3

[limits]
time = "1000ms"
memory = "256MB"

[[subtasks]]
id = 1
score = 100.0
tests = ["1"]
"#;
    let err = uj_core::config::ProblemConfig::from_toml_str(problem_toml, Some(root)).unwrap_err();
    assert!(
        err.message.contains("Multi-participant communication"),
        "Error message must explain requirement for explicit topology: {}",
        err.message
    );
}

/// REG-MCP-001: MCP failure semantics, fail-closed pipeline/contest, and cancellation
#[test]
fn test_reg_mcp_001_fail_closed_and_cancellation() {
    let mut child = Command::new(uj_bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn uj mcp");

    let mut stdin = child.stdin.take().expect("Failed to open stdin");
    let stdout = child.stdout.take().expect("Failed to open stdout");
    let mut reader = BufReader::new(stdout);

    // Initialize
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {}
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&init_req).unwrap()).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();

    // 1. Pipeline on non-existent or empty problem -> must return isError: true
    let temp_empty = tempdir().unwrap();
    let pipe_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "pipeline",
            "arguments": {
                "problem": temp_empty.path().to_str().unwrap()
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&pipe_req).unwrap()).unwrap();
    stdin.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let pipe_resp: serde_json::Value = serde_json::from_str(&line).expect("Valid JSON response");
    assert_eq!(pipe_resp["id"], 2);
    assert_eq!(
        pipe_resp["result"]["isError"], true,
        "Pipeline on empty directory without problem.toml must fail with isError=true"
    );

    // 2. Contest on empty directory -> must return isError: true
    let contest_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "contest",
            "arguments": {
                "dir": temp_empty.path().to_str().unwrap()
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&contest_req).unwrap()).unwrap();
    stdin.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let contest_resp: serde_json::Value = serde_json::from_str(&line).expect("Valid JSON response");
    assert_eq!(contest_resp["id"], 3);
    assert_eq!(
        contest_resp["result"]["isError"], true,
        "Contest on empty directory with no problems must fail with isError=true"
    );

    // 3. init_problem -> must return isError: false and create problem.toml
    let temp_init = tempdir().unwrap();
    let init_prob_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "init_problem",
            "arguments": {
                "dir": temp_init.path().to_str().unwrap()
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&init_prob_req).unwrap()).unwrap();
    stdin.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let init_prob_resp: serde_json::Value =
        serde_json::from_str(&line).expect("Valid JSON response");
    assert_eq!(init_prob_resp["id"], 4);
    assert_eq!(init_prob_resp["result"]["isError"], false);
    assert!(temp_init.path().join("problem.toml").exists());

    // 4. Invalid jsonrpc version -> must return JSON-RPC error -32600
    let invalid_rpc = serde_json::json!({
        "jsonrpc": "1.0",
        "id": 5,
        "method": "ping"
    });
    writeln!(stdin, "{}", serde_json::to_string(&invalid_rpc).unwrap()).unwrap();
    stdin.flush().unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let err_resp: serde_json::Value = serde_json::from_str(&line).expect("Valid JSON response");
    assert_eq!(err_resp["error"]["code"], -32600);

    drop(stdin);
    let _ = child.wait();
}

/// REG-REPORT-001: HTML escaping against script injection, unique DOM IDs,
/// and atomic report store persistence.
#[test]
fn test_reg_report_001_html_escaping_and_atomic_store() {
    let report = SubmissionJudgeReport {
        schema_version: 1,
        problem: "test_escape".to_string(),
        submission: "sol.cpp".to_string(),
        verdict: Verdict::Accepted,
        score: 100.0,
        max_time_ms: 10,
        max_memory_kb: 1024,
        subtasks: vec![],
        tests: vec![SingleTestResult {
            id: "<svg onload=alert('xss')>".to_string(),
            subtask: None,
            verdict: Verdict::Accepted,
            score: 100.0,
            time_ms: 5,
            wall_ms: 6,
            memory_kb: 512,
            exit: Default::default(),
            message: Some("Note: <script>bad()</script>".to_string()),
            checker: Some("Found <img src=x> in answer".to_string()),
            diff: None,
            timing: Default::default(),
            sandbox_tier: None,
        }],
    };

    let html = uj_report::generate_html(&report).expect("HTML generation must succeed");

    // 1. Verify dangerous payloads are properly escaped and never inserted raw
    assert!(
        !html.contains("<svg onload=alert('xss')>"),
        "Raw SVG tag must NOT be present in generated HTML"
    );
    assert!(
        html.contains("&lt;svg onload=alert(&#39;xss&#39;)&gt;")
            || html.contains("&lt;svg onload=alert('xss')&gt;"),
        "Escaped SVG tag must be present in HTML"
    );
    assert!(
        !html.contains("<script>bad()</script>"),
        "Raw script tag in message must NOT be present in generated HTML"
    );

    // 2. Verify row detail DOM ID is safe and does not include raw payloads
    assert!(
        !html.contains("test-detail-0-<svg"),
        "DOM ID must not interpolate unescaped test ID"
    );
    assert!(
        html.contains("test-detail-row-1"),
        "Sanitized numeric row detail ID must be present"
    );

    // 3. Verify timestamp is dynamic UTC and not hardcoded stale time
    assert!(
        !html.contains("2026-10-10 16:30"),
        "HTML report must not use hardcoded stale timestamp"
    );
    assert!(
        html.contains("UTC"),
        "HTML report must include UTC timestamp"
    );

    // 4. Verify atomic persistence in .uj/runs
    let temp = tempdir().unwrap();
    let root = temp.path();

    let problem_toml = r#"
schema = 1
name = "atomic_report_test"
title = "Atomic Report Test"
type = "traditional"

[limits]
time = "1000ms"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"
"#;
    fs::write(root.join("problem.toml"), problem_toml).unwrap();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();
    fs::write(tests_dir.join("1.in"), "1 2\n").unwrap();
    fs::write(tests_dir.join("1.out"), "3\n").unwrap();

    let sol = root.join("sol.cpp");
    fs::write(&sol, "#include <iostream>\nint main() { int a, b; if (std::cin >> a >> b) std::cout << a+b << std::endl; return 0; }\n").unwrap();

    let out = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to execute uj judge");

    assert!(out.status.success());

    let runs_dir = root.join(".uj").join("runs");
    assert!(runs_dir.exists(), ".uj/runs directory must exist");
    let last_json = runs_dir.join("last.json");
    assert!(last_json.exists(), ".uj/runs/last.json must exist");

    let last_content = fs::read_to_string(&last_json).unwrap();
    let last_rep: serde_json::Value = serde_json::from_str(&last_content).unwrap();
    assert_eq!(last_rep["problem"], "atomic_report_test");
    assert_eq!(last_rep["verdict"], "AC");

    // Also verify historical run-*.json was created
    let mut found_history = false;
    for entry in fs::read_dir(&runs_dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("run-") && name.ends_with(".json") {
            found_history = true;
            break;
        }
    }
    assert!(
        found_history,
        "Monotonic run-*.json history file must be created"
    );
}

/// REG-NDJSON-001: NDJSON event stream must not have premature summary,
/// must emit subtask_done, submission_done, and final summary with actual verdict/score.
#[test]
fn test_reg_ndjson_001_event_sequence_and_final_summary() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();

    let fixture = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&fixture, root).unwrap();

    let sol = root.join("solutions/ac.cpp");

    let out = Command::new(uj_bin())
        .arg("judge")
        .arg(&sol)
        .arg("-p")
        .arg(root)
        .arg("--format")
        .arg("ndjson")
        .output()
        .expect("failed to execute uj judge with ndjson");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "NDJSON output must contain lines");

    let mut events = Vec::new();
    for line in &lines {
        let val: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|_| {
            panic!("Invalid NDJSON line: {line}");
        });
        let ev = val["event"].as_str().unwrap().to_string();
        events.push((ev, val));
    }

    // Verify first event is NOT summary
    assert_ne!(
        events[0].0, "summary",
        "First NDJSON event must not be a premature summary!"
    );

    // Verify subtask_done is emitted
    let has_subtask_done = events.iter().any(|(ev, _)| ev == "subtask_done");
    assert!(has_subtask_done, "NDJSON stream must emit subtask_done");

    // Verify submission_done is emitted
    let has_sub_done = events.iter().any(|(ev, _)| ev == "submission_done");
    assert!(has_sub_done, "NDJSON stream must emit submission_done");

    // Verify final event is summary
    let last = events.last().unwrap();
    assert_eq!(last.0, "summary", "Final NDJSON event must be summary");
    assert_eq!(last.1["data"]["final_verdict"], "AC");
    assert_eq!(last.1["data"]["final_score"], 100.0);
}

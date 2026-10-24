use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tempfile::tempdir;
use uj_core::verdict::Verdict;
use uj_report::result::SubmissionJudgeReport;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn uj_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(if cfg!(windows) { "uj.exe" } else { "uj" });
    path
}

fn write_aplusb_solution(dir: &Path) -> PathBuf {
    let sol = dir.join("ac.cpp");
    fs::write(
        &sol,
        r#"
#include <iostream>
int main() {
    long long a, b;
    if (std::cin >> a >> b) {
        std::cout << (a + b) << "\n";
    }
    return 0;
}
"#,
    )
    .unwrap();
    sol
}

/// Verify that 3, 6, 7, and 12 test cases with no subtasks and all-AC tests
/// produce Verdict::Accepted and score == 100.0 end-to-end.
#[test]
fn test_e2e_scoring_3_6_7_12_tests_all_ac() {
    let _guard = lock_test();

    for count in [3usize, 6, 7, 12] {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let tests_dir = root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(
            root.join("problem.toml"),
            r#"
schema = 1
name = "fixed_score_test"
type = "traditional"

[limits]
time = "1s"
memory = "256MiB"
"#,
        )
        .unwrap();

        for i in 1..=count {
            fs::write(
                tests_dir.join(format!("{i}.in")),
                format!("{i} {}\n", i * 2),
            )
            .unwrap();
            fs::write(tests_dir.join(format!("{i}.ans")), format!("{}\n", i * 3)).unwrap();
        }

        let sol = write_aplusb_solution(root);
        let output = Command::new(uj_bin())
            .args([
                "judge",
                sol.to_str().unwrap(),
                "-p",
                root.to_str().unwrap(),
                "--format",
                "json",
                "--precise",
                "false",
            ])
            .current_dir(root)
            .output()
            .expect("failed to run uj judge");

        assert!(
            output.status.success(),
            "uj judge failed for {count} tests: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: SubmissionJudgeReport = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report.verdict,
            Verdict::Accepted,
            "Expected Accepted for {count} tests, got {:?}",
            report.verdict
        );
        assert_eq!(
            report.score, 100.0,
            "Expected 100.0 for {count} tests, got {}",
            report.score
        );
    }
}

/// Verify that a Sum subtask with score=10 and 3 tests all-AC produces Verdict::Accepted,
/// score == 10.0, and does NOT skip a dependent subtask.
#[test]
fn test_e2e_sum_subtask_10pts_3_tests_and_dependent() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();

    fs::write(
        root.join("problem.toml"),
        r#"
schema = 1
name = "sum_dep_test"
type = "traditional"

[limits]
time = "1s"
memory = "256MiB"

[[subtasks]]
id = 1
score = 10.0
scoring = "sum"
tests = ["1-3"]

[[subtasks]]
id = 2
score = 90.0
scoring = "min"
tests = ["4"]
depends = [1]
"#,
    )
    .unwrap();

    for i in 1..=4 {
        fs::write(tests_dir.join(format!("{i}.in")), format!("{i} {i}\n")).unwrap();
        fs::write(tests_dir.join(format!("{i}.ans")), format!("{}\n", i * 2)).unwrap();
    }

    let sol = write_aplusb_solution(root);
    let output = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .expect("failed to run uj judge");

    assert!(output.status.success());
    let report: SubmissionJudgeReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.subtasks.len(), 2);
    assert_eq!(report.subtasks[0].verdict, Verdict::Accepted);
    assert_eq!(report.subtasks[0].score, 10.0);
    assert_eq!(report.subtasks[1].verdict, Verdict::Accepted);
    assert_eq!(report.subtasks[1].score, 90.0);
    assert_eq!(report.verdict, Verdict::Accepted);
    assert_eq!(report.score, 100.0);
}

/// Verify that when the interactor finishes first with Accepted, solution crashes or
/// resource limit violations (RE / TLE) are still detected and reported instead of AC.
/// Also verify that scanning `..` from inside `box_dir` cannot locate or spoof `trusted_dir`.
#[test]
fn test_e2e_interactive_post_ac_crash_tle_and_trusted_dir_isolation() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();

    // Interactor reads one integer from contestant; if it equals 42, writes AC XML and exits 0 immediately.
    fs::write(
        root.join("interactor.cpp"),
        r#"
#include <fstream>
#include <iostream>
#include <string>

int main(int argc, char* argv[]) {
    // argv: [exe, input, tout, ans, result_xml, -appes]
    std::string xml_path = (argc >= 5) ? argv[4] : "";
    int guess = 0;
    if (!(std::cin >> guess)) {
        if (!xml_path.empty()) {
            std::ofstream xml(xml_path);
            xml << "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
                << "<result outcome=\"wrong-answer\">No output</result>\n";
        }
        return 1;
    }
    if (guess == 42) {
        if (!xml_path.empty()) {
            std::ofstream xml(xml_path);
            xml << "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
                << "<result outcome=\"accepted\">ok</result>\n";
        }
        return 0;
    } else {
        if (!xml_path.empty()) {
            std::ofstream xml(xml_path);
            xml << "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
                << "<result outcome=\"wrong-answer\">wrong guess</result>\n";
        }
        return 1;
    }
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("problem.toml"),
        r#"
schema = 1
name = "interactive_checks"
type = "interactive"
interactor = "interactor.cpp"

[limits]
time = "400ms"
memory = "256MiB"
"#,
    )
    .unwrap();

    fs::write(tests_dir.join("1.in"), "42\n").unwrap();
    fs::write(tests_dir.join("1.ans"), "42\n").unwrap();

    // 1. Solution outputs 42 (interactor exits 0 with AC), then exits with code 7 -> must be RE!
    let sol_re = root.join("sol_re.cpp");
    fs::write(
        &sol_re,
        r#"
#include <iostream>
#include <cstdlib>
int main() {
    std::cout << 42 << std::endl;
    std::exit(7);
}
"#,
    )
    .unwrap();

    let out_re = Command::new(uj_bin())
        .args([
            "judge",
            sol_re.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep_re: SubmissionJudgeReport = serde_json::from_slice(&out_re.stdout).unwrap();
    assert_eq!(
        rep_re.verdict,
        Verdict::RuntimeError,
        "Solution exiting non-zero after interactor AC must be RuntimeError, got {:?}",
        rep_re.verdict
    );
    assert_eq!(rep_re.score, 0.0);

    // 2. Solution outputs 42 (interactor exits 0 with AC), then busy-loops past CPU limit -> must be TLE!
    let sol_tle = root.join("sol_tle.cpp");
    fs::write(
        &sol_tle,
        r#"
#include <iostream>
int main() {
    std::cout << 42 << std::endl;
    volatile unsigned long long x = 0;
    while (true) { x++; }
    return 0;
}
"#,
    )
    .unwrap();

    let out_tle = Command::new(uj_bin())
        .args([
            "judge",
            sol_tle.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep_tle: SubmissionJudgeReport = serde_json::from_slice(&out_tle.stdout).unwrap();
    assert_eq!(
        rep_tle.verdict,
        Verdict::TimeLimitExceeded,
        "Solution exceeding CPU time after interactor AC must be TLE, got {:?}",
        rep_tle.verdict
    );
    assert_eq!(rep_tle.score, 0.0);

    // 3. Solution tries to scan `..` (parent of box_dir) to overwrite any XML result file, and outputs wrong guess 99.
    let sol_spoof_parent = root.join("sol_spoof_parent.cpp");
    fs::write(
        &sol_spoof_parent,
        r#"
#include <filesystem>
#include <fstream>
#include <iostream>

namespace fs = std::filesystem;

int main() {
    std::error_code ec;
    for (auto const& entry : fs::directory_iterator("..", ec)) {
        if (ec) break;
        auto name = entry.path().filename().string();
        if (name.find("trusted_int_") == 0 && entry.is_directory(ec)) {
            for (auto const& sub : fs::directory_iterator(entry.path(), ec)) {
                if (ec) break;
                if (sub.path().extension() == ".xml") {
                    std::ofstream out(sub.path());
                    out << "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
                        << "<result outcome=\"accepted\">spoofed parent</result>\n";
                }
            }
        }
    }
    std::cout << 99 << std::endl;
    return 0;
}
"#,
    )
    .unwrap();

    let out_spoof = Command::new(uj_bin())
        .args([
            "judge",
            sol_spoof_parent.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep_spoof: SubmissionJudgeReport = serde_json::from_slice(&out_spoof.stdout).unwrap();
    assert_eq!(
        rep_spoof.verdict,
        Verdict::WrongAnswer,
        "Contestant scanning .. must not be able to spoof interactor XML"
    );
    assert_eq!(rep_spoof.score, 0.0);
}

/// Verify that communication judging fails closed (JudgeFailed) when the manager writes no score
/// or writes a score outside 0.0..=1.0, and does not scrape arbitrary numbers from stderr.
#[test]
fn test_e2e_communication_score_fail_closed() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();

    // Manager reads mode from test input:
    // mode 1: exits 0 without writing score to UJ_SCORE_FILE or stdout (writes "1.0" to stderr) -> must be JudgeFailed!
    // mode 2: writes "100" (invalid ratio > 1.0) to UJ_SCORE_FILE -> must be JudgeFailed!
    // mode 3: writes "1.0" to UJ_SCORE_FILE -> must be Accepted (100.0)!
    fs::write(
        root.join("manager.cpp"),
        r#"
#include <fstream>
#include <iostream>

int main(int argc, char* argv[]) {
    std::ifstream in(argv[1]);
    int mode = 0;
    in >> mode;
    int x = 0;
    std::cout << mode << std::endl;
    std::cin >> x;
    if (mode == 1) {
        std::cerr << "1.0\n";
        return 0;
    } else if (mode == 2) {
        if (argc >= 3) {
            std::ofstream sf(argv[2]);
            sf << "100\n";
        }
        return 0;
    } else {
        if (argc >= 3) {
            std::ofstream sf(argv[2]);
            sf << "1.0\n";
        }
        return 0;
    }
}
"#,
    )
    .unwrap();

    let sol = root.join("participant.cpp");
    fs::write(
        &sol,
        r#"
#include <iostream>
int main() {
    int m = 0;
    if (std::cin >> m) {
        std::cout << m << std::endl;
    }
    return 0;
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("problem.toml"),
        r#"
schema = 1
name = "comm_strict"
type = "communication"
manager = "manager.cpp"
processes = 1

[limits]
time = "1s"
memory = "256MiB"
"#,
    )
    .unwrap();

    // Case 1: Missing score (only stderr has "1.0") -> JudgeFailed
    fs::write(tests_dir.join("1.in"), "1\n").unwrap();
    fs::write(tests_dir.join("1.ans"), "1\n").unwrap();
    let out1 = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep1: SubmissionJudgeReport = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(rep1.verdict, Verdict::JudgeFailed);
    assert_eq!(rep1.score, 0.0);

    // Case 2: Out-of-range score "100" -> JudgeFailed
    fs::write(tests_dir.join("1.in"), "2\n").unwrap();
    let out2 = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep2: SubmissionJudgeReport = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(rep2.verdict, Verdict::JudgeFailed);
    assert_eq!(rep2.score, 0.0);

    // Case 3: Valid score "1.0" -> Accepted (100.0)
    fs::write(tests_dir.join("1.in"), "3\n").unwrap();
    let out3 = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
            "--precise",
            "false",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    let rep3: SubmissionJudgeReport = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(rep3.verdict, Verdict::Accepted);
    assert_eq!(rep3.score, 100.0);
}

/// Verify that overlapping glob subtask patterns and subtasks matching 0 discovered tests
/// are rejected during judging with a configuration error.
#[test]
fn test_e2e_overlapping_and_zero_match_subtasks_rejected() {
    let _guard = lock_test();
    let temp = tempdir().unwrap();
    let root = temp.path();
    let tests_dir = root.join("tests");
    fs::create_dir_all(&tests_dir).unwrap();

    for i in 1..=3 {
        fs::write(tests_dir.join(format!("{i}.in")), format!("{i} {i}\n")).unwrap();
        fs::write(tests_dir.join(format!("{i}.ans")), format!("{}\n", i * 2)).unwrap();
    }
    let sol = write_aplusb_solution(root);

    // 1. Overlapping globs ("*" and "1*")
    fs::write(
        root.join("problem.toml"),
        r#"
schema = 1
name = "overlap_prob"
type = "traditional"

[limits]
time = "1s"
memory = "256MiB"

[[subtasks]]
id = 1
score = 50.0
tests = ["*"]

[[subtasks]]
id = 2
score = 50.0
tests = ["1*"]
"#,
    )
    .unwrap();

    let out_overlap = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        !out_overlap.status.success(),
        "Overlapping subtask patterns must be rejected by uj judge"
    );

    // 2. Subtask matching 0 discovered tests ("99*")
    fs::write(
        root.join("problem.toml"),
        r#"
schema = 1
name = "zero_match_prob"
type = "traditional"

[limits]
time = "1s"
memory = "256MiB"

[[subtasks]]
id = 1
score = 50.0
tests = ["1-3"]

[[subtasks]]
id = 2
score = 50.0
tests = ["99*"]
"#,
    )
    .unwrap();

    let out_zero = Command::new(uj_bin())
        .args([
            "judge",
            sol.to_str().unwrap(),
            "-p",
            root.to_str().unwrap(),
            "--format",
            "json",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        !out_zero.status.success(),
        "Subtask matching 0 discovered tests must be rejected by uj judge"
    );
}

use std::fs;
use std::process::Command;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn get_uj_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
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
fn test_authoring_init_and_pipeline() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_p6_init");
    let _ = fs::remove_dir_all(&test_dir);

    // 1. uj init
    let init_out = uj_cmd()
        .arg("init")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj init");

    assert_eq!(
        init_out.status.code(),
        Some(0),
        "uj init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    assert!(test_dir.join("problem.toml").is_file());
    assert!(test_dir.join("statement.md").is_file());
    assert!(test_dir.join("testlib.h").is_file());
    assert!(test_dir.join("gen.cpp").is_file());
    assert!(test_dir.join("validator.cpp").is_file());
    assert!(test_dir.join("solutions/std.cpp").is_file());
    assert!(test_dir.join("solutions/brute.cpp").is_file());
    assert!(test_dir.join("solutions/wrong.cpp").is_file());

    // 2. uj pipeline
    let pipe_out = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pipeline");

    assert_eq!(
        pipe_out.status.code(),
        Some(0),
        "uj pipeline failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&pipe_out.stdout),
        String::from_utf8_lossy(&pipe_out.stderr)
    );

    // 3. Test caching & modifying std
    let std_path = test_dir.join("solutions/std.cpp");
    let mod_std = r#"#include <iostream>
using namespace std;
int main() {
    long long a, b;
    if (cin >> a >> b) {
        long long sum = a + b;
        cout << sum << "\n";
    }
    return 0;
}
"#;
    fs::write(&std_path, mod_std).expect("Failed to modify std.cpp");

    let ans_out = uj_cmd()
        .arg("ans")
        .arg("-p")
        .arg(&test_dir)
        .arg("--force")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj ans");

    assert_eq!(
        ans_out.status.code(),
        Some(0),
        "uj ans failed after modifying std: {}",
        String::from_utf8_lossy(&ans_out.stderr)
    );

    let verify_out = uj_cmd()
        .arg("verify")
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj verify");

    assert_eq!(
        verify_out.status.code(),
        Some(0),
        "uj verify failed after updating std: {}",
        String::from_utf8_lossy(&verify_out.stderr)
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_authoring_stress() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_p6_stress");
    let _ = fs::remove_dir_all(&test_dir);

    // Init problem
    let _ = uj_cmd()
        .arg("init")
        .arg(&test_dir)
        .output()
        .expect("Failed to execute uj init");

    // Run stress test comparing solutions/wrong.cpp vs solutions/brute.cpp
    let stress_out = uj_cmd()
        .arg("stress")
        .arg("-p")
        .arg(&test_dir)
        .arg("--gen")
        .arg("gen.cpp")
        .arg("--sol")
        .arg("solutions/wrong.cpp")
        .arg("--brute")
        .arg("solutions/brute.cpp")
        .arg("-n")
        .arg("20")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj stress");

    // wrong.cpp outputs a+b+1 while brute outputs a+b, so it MUST fail with exit code 1
    assert_eq!(
        stress_out.status.code(),
        Some(1),
        "Expected stress test to find bug in wrong.cpp. Output: {}",
        String::from_utf8_lossy(&stress_out.stdout)
    );

    let stdout_str = String::from_utf8_lossy(&stress_out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str)
        .unwrap_or_else(|_| panic!("Failed to parse JSON stress result: {stdout_str}"));
    assert_eq!(report["ok"], false);
    assert!(test_dir.join("stress").is_dir());

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_authoring_doctor_problem() {
    let _guard = lock_test();
    let doctor_out = uj_cmd()
        .arg("doctor")
        .arg("--problem")
        .arg("--format")
        .arg("json")
        .current_dir(get_workspace_root().join("tests/problems/aplusb"))
        .output()
        .expect("Failed to execute uj doctor --problem");

    assert_eq!(
        doctor_out.status.code(),
        Some(0),
        "uj doctor --problem failed: {}",
        String::from_utf8_lossy(&doctor_out.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&doctor_out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["ok"], true);
}

fn copy_dir_all(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
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
fn test_verify_parallel_order() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_verify_order");
    let _ = fs::remove_dir_all(&test_dir);

    let aplusb_src = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&aplusb_src, &test_dir).unwrap();

    let p_toml = test_dir.join("problem.toml");
    let mut toml_str = fs::read_to_string(&p_toml).unwrap();
    toml_str.push_str(
        r#"
[[solutions]]
path = "solutions/ac.cpp"
tag = "accepted"

[[solutions]]
path = "solutions/wa.cpp"
tag = "wrong_answer"

[[solutions]]
path = "solutions/tle.cpp"
tag = "time_limit"
"#,
    );
    fs::write(&p_toml, toml_str).unwrap();

    let out = uj_cmd()
        .arg("verify")
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("human")
        .output()
        .expect("Failed to execute uj verify");

    assert_eq!(
        out.status.code(),
        Some(0),
        "uj verify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout_str.lines().filter(|l| l.starts_with("[")).collect();

    assert_eq!(lines.len(), 3, "Expected 3 solution results: {stdout_str}");
    assert!(
        lines[0].contains("[1/3] solutions/ac.cpp (Accepted) -> OK"),
        "Line 1 mismatch: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("[2/3] solutions/wa.cpp (WrongAnswer) -> OK"),
        "Line 2 mismatch: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("[3/3] solutions/tle.cpp (TimeLimit) -> OK"),
        "Line 3 mismatch: {}",
        lines[2]
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_communication_star_topology() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_comm_star");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(test_dir.join("tests")).unwrap();
    fs::create_dir_all(test_dir.join("solutions")).unwrap();

    fs::write(
        test_dir.join("problem.toml"),
        r#"name = "star_comm"
type = "communication"
manager = "manager.cpp"
processes = 2
communication_topology = "star"

[limits]
time = "2000ms"
memory = "256MB"

[checker]
type = "builtin"
name = "wcmp"
"#,
    )
    .unwrap();

    fs::write(test_dir.join("tests/1.in"), "15 27\n").unwrap();
    fs::write(test_dir.join("tests/1.ans"), "42\n").unwrap();

    // Manager reads a, b from input file argv[1], sends `a` to worker 0 and `b` to worker 1 via star pipes,
    // reads 2*a from worker 0 and 2*b from worker 1, and prints 1.0 (AC) to score_file / stdout if (r0 + r1)/2 == a + b.
    let manager_cpp = r#"#include <cstdio>
#include <cstdlib>
#ifdef _WIN32
#include <io.h>
#define fdopen _fdopen
#else
#include <unistd.h>
#endif

FILE* open_pipe_handle(const char* arg, const char* mode) {
    long long raw = std::strtoll(arg, nullptr, 10);
#ifdef _WIN32
    int fd = _open_osfhandle((intptr_t)raw, 0);
#else
    int fd = (int)raw;
#endif
    return fdopen(fd, mode);
}

int main(int argc, char** argv) {
    if (argc < 7) return 2;
    FILE* fin = std::fopen(argv[1], "r");
    if (!fin) return 3;
    long long a = 0, b = 0;
    if (std::fscanf(fin, "%lld %lld", &a, &b) != 2) return 3;
    std::fclose(fin);

    FILE* from_w0 = open_pipe_handle(argv[3], "r");
    FILE* to_w0 = open_pipe_handle(argv[4], "w");
    FILE* from_w1 = open_pipe_handle(argv[5], "r");
    FILE* to_w1 = open_pipe_handle(argv[6], "w");
    if (!to_w0 || !from_w0 || !to_w1 || !from_w1) return 4;

    std::fprintf(to_w0, "%lld\n", a);
    std::fflush(to_w0);
    std::fprintf(to_w1, "%lld\n", b);
    std::fflush(to_w1);

    long long r0 = 0, r1 = 0;
    if (std::fscanf(from_w0, "%lld", &r0) != 1) return 5;
    if (std::fscanf(from_w1, "%lld", &r1) != 1) return 6;

    double score = ((r0 + r1) / 2 == a + b) ? 1.0 : 0.0;
    std::printf("%.1f\n", score);
    std::fflush(stdout);
    return 0;
}
"#;
    fs::write(test_dir.join("manager.cpp"), manager_cpp).unwrap();

    let worker_cpp = r#"#include <iostream>
int main() {
    long long x;
    if (std::cin >> x) {
        std::cout << (x * 2) << std::endl;
    }
    return 0;
}
"#;
    let sol_path = test_dir.join("solutions/ac.cpp");
    fs::write(&sol_path, worker_cpp).unwrap();

    let out = uj_cmd()
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj judge on star communication problem");

    assert_eq!(
        out.status.code(),
        Some(0),
        "Star communication judge failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(report["verdict"], "AC");
    assert_eq!(report["score"], 100.0);

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_doctor_subtask_overlap_and_orphan_ans() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_doctor_overlap");
    let _ = fs::remove_dir_all(&test_dir);
    fs::create_dir_all(test_dir.join("tests")).unwrap();

    fs::write(
        test_dir.join("problem.toml"),
        r#"name = "doc_test"
type = "traditional"

[limits]
time = "1000ms"
memory = "256MB"

[[subtasks]]
id = 1
score = 30.0
tests = ["*"]

[[subtasks]]
id = 2
score = 40.0
tests = ["2*"]

[[subtasks]]
id = 3
score = 30.0
tests = ["nonexistent_*"]
"#,
    )
    .unwrap();

    fs::write(test_dir.join("tests/1.in"), "1 2\n").unwrap();
    fs::write(test_dir.join("tests/1.ans"), "3\n").unwrap();
    fs::write(test_dir.join("tests/2.in"), "3 4\n").unwrap();
    fs::write(test_dir.join("tests/2.ans"), "7\n").unwrap();
    fs::write(test_dir.join("tests/orphan.ans"), "99\n").unwrap();

    let out = uj_cmd()
        .arg("doctor")
        .arg("--problem")
        .arg("--format")
        .arg("json")
        .current_dir(&test_dir)
        .output()
        .expect("Failed to execute uj doctor --problem");

    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let checks = report["checks"].as_array().unwrap();

    let orphan_check = checks
        .iter()
        .find(|c| c["name"] == "Orphan Answer Files")
        .expect("Orphan Answer Files check missing");
    assert_eq!(orphan_check["status"], "Warn");
    assert!(
        orphan_check["message"]
            .as_str()
            .unwrap()
            .contains("orphan.ans")
    );

    let coverage_check = checks
        .iter()
        .find(|c| c["name"] == "Subtask Coverage")
        .expect("Subtask Coverage check missing");
    assert_eq!(coverage_check["status"], "Warn");
    assert!(
        coverage_check["message"]
            .as_str()
            .unwrap()
            .contains("Subtasks matching 0 test cases: [3]")
    );

    let overlap_check = checks
        .iter()
        .find(|c| c["name"] == "Subtask Pattern Overlap")
        .expect("Subtask Pattern Overlap check missing");
    assert_eq!(overlap_check["status"], "Warn");
    assert!(
        overlap_check["message"]
            .as_str()
            .unwrap()
            .contains("subtasks [1, 2]")
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_tl_subtask_stats_and_preserve_indentation() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_tl_indent");
    let _ = fs::remove_dir_all(&test_dir);

    let aplusb_src = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&aplusb_src, &test_dir).unwrap();

    let p_toml = test_dir.join("problem.toml");
    fs::write(
        &p_toml,
        r#"name = "aplusb"
type = "traditional"

[limits]
    time = "1000ms" # indented time comment
    memory = "256MB"

[[subtasks]]
id = 1
score = 100.0
tests = ["*"]

[[solutions]]
path = "solutions/ac.cpp"
tag = "main"
"#,
    )
    .unwrap();

    let out = uj_cmd()
        .arg("tl")
        .arg("-p")
        .arg(&test_dir)
        .arg("--apply")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj tl --apply");

    assert_eq!(
        out.status.code(),
        Some(0),
        "uj tl failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let res: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(res["ok"], true);
    assert_eq!(res["applied"], true);
    let subtask_stats = res["subtask_stats"]
        .as_array()
        .expect("subtask_stats missing");
    assert_eq!(subtask_stats.len(), 1);
    assert_eq!(subtask_stats[0]["id"], 1);

    let updated_toml = fs::read_to_string(&p_toml).unwrap();
    assert!(
        updated_toml.contains("    time = \""),
        "Expected leading 4-space indentation to be preserved in:\n{updated_toml}"
    );
    assert!(
        updated_toml.contains("# indented time comment"),
        "Expected trailing inline comment to be preserved in:\n{updated_toml}"
    );

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_r12_verify_aliases_mixed_wa_tle_and_json_matrix_and_tl() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_r12_verify_matrix");
    let _ = fs::remove_dir_all(&test_dir);

    let aplusb_src = get_workspace_root().join("tests/problems/aplusb");
    copy_dir_all(&aplusb_src, &test_dir).unwrap();

    // Create a mixed WA+TLE solution: on test 1 (1 2), outputs wrong answer; on test 2 (10 20), loops forever
    let mixed_cpp = r#"#include <iostream>
int main() {
    long long a, b;
    if (std::cin >> a >> b) {
        if (a == 1 && b == 2) {
            std::cout << -999 << "\n";
            return 0;
        }
        volatile long long s = 0;
        while (true) { s += a; }
    }
    return 0;
}
"#;
    fs::write(test_dir.join("solutions/mixed_wa_tle.cpp"), mixed_cpp).unwrap();

    let p_toml = test_dir.join("problem.toml");
    fs::write(
        &p_toml,
        r#"name = "aplusb_r12"
type = "traditional"

[limits]
time = "500ms"
memory = "256MB"

[[subtasks]]
id = 1
score = 100.0
tests = ["*"]

[[solutions]]
path = "solutions/ac.cpp"
tag = "main"

[[solutions]]
path = "solutions/wa.cpp"
tag = "wa"

[[solutions]]
path = "solutions/tle.cpp"
tag = "tle"

[[solutions]]
path = "solutions/mixed_wa_tle.cpp"
tag = "wa_or_tle"
"#,
    )
    .unwrap();

    let out = uj_cmd()
        .arg("verify")
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj verify --format json");

    assert_eq!(
        out.status.code(),
        Some(0),
        "uj verify failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let res: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(res["ok"], true);
    assert_eq!(res["verified"], 4);

    let sols = res["solutions"]
        .as_array()
        .expect("solutions array missing");
    assert_eq!(sols.len(), 4);

    assert_eq!(sols[0]["path"], "solutions/ac.cpp");
    assert_eq!(sols[0]["tag"], "main");
    assert_eq!(sols[0]["matches_tag"], true);
    assert_eq!(sols[0]["verdict"], "AC");
    assert!(sols[0]["max_time_test"].is_string());
    assert!(sols[0]["first_fail_test"].is_null());

    assert_eq!(sols[3]["path"], "solutions/mixed_wa_tle.cpp");
    assert_eq!(sols[3]["tag"], "wa_or_tle");
    assert_eq!(sols[3]["matches_tag"], true);
    assert!(sols[3]["first_fail_test"].is_string());
    assert!(sols[3]["first_fail_verdict"].is_string());

    // Verify cache file was written for uj tl reuse
    assert!(test_dir.join(".uj/cache/verify_tl_cache.json").is_file());

    // Run uj tl --format json and verify slowest test IDs are reported
    let tl_out = uj_cmd()
        .arg("tl")
        .arg("-p")
        .arg(&test_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj tl --format json");

    assert_eq!(
        tl_out.status.code(),
        Some(0),
        "uj tl failed: {}",
        String::from_utf8_lossy(&tl_out.stderr)
    );
    let tl_res: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&tl_out.stdout)).unwrap();
    assert_eq!(tl_res["ok"], true);
    assert!(tl_res["main_slowest_test"].is_string());
    assert!(tl_res["slowest_ac_test"].is_string());
    assert!(tl_res["peak_memory_test"].is_string());

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_r12_stress_empty_stdout_and_gen_args_and_env() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_r12_stress_gen_args");
    let _ = fs::remove_dir_all(&test_dir);

    let _ = uj_cmd()
        .arg("init")
        .arg(&test_dir)
        .output()
        .expect("Failed to execute uj init");

    // Generator that mimics T10 fixed-ID dispatch unless argv[1] == "random" and UJ_STRESS == "1"
    let custom_gen = r#"#include <cstdlib>
#include <cstring>
#include <iostream>

int main(int argc, char* argv[]) {
    if (argc >= 3 && std::strcmp(argv[1], "random") == 0) {
        const char* stress_env = std::getenv("UJ_STRESS");
        if (stress_env && std::strcmp(stress_env, "1") == 0) {
            long long seed = std::atoll(argv[2]);
            std::cout << (seed % 100) << " " << ((seed * 7) % 100) << "\n";
            return 0;
        }
    }
    int tid = (argc >= 2) ? std::atoi(argv[1]) : 0;
    if (tid >= 1 && tid <= 3) {
        std::cout << tid << " " << (tid + 1) << "\n";
    }
    // Outside 1..3, outputs nothing and exits 0!
    return 0;
}
"#;
    fs::write(test_dir.join("gen.cpp"), custom_gen).unwrap();

    // 1. Calling uj stress with --seed 100 (default --gen-args "{seed}") must detect empty stdout immediately
    let fail_out = uj_cmd()
        .arg("stress")
        .arg("-p")
        .arg(&test_dir)
        .arg("--gen")
        .arg("gen.cpp")
        .arg("--sol")
        .arg("solutions/std.cpp")
        .arg("--brute")
        .arg("solutions/brute.cpp")
        .arg("-n")
        .arg("1")
        .arg("--seed")
        .arg("100")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj stress");

    assert_ne!(fail_out.status.code(), Some(0));
    let fail_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&fail_out.stdout)).unwrap();
    assert_eq!(fail_json["ok"], false);
    assert_eq!(fail_json["error"]["kind"], "GeneratorFailed");
    assert!(
        fail_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Generator produced empty stdout on seed 100")
    );

    // 2. Calling uj stress with --gen-args "random {seed}" and --seed 100 succeeds
    let ok_out = uj_cmd()
        .arg("stress")
        .arg("-p")
        .arg(&test_dir)
        .arg("--gen")
        .arg("gen.cpp")
        .arg("--sol")
        .arg("solutions/std.cpp")
        .arg("--brute")
        .arg("solutions/brute.cpp")
        .arg("-n")
        .arg("5")
        .arg("--seed")
        .arg("100")
        .arg("--gen-args")
        .arg("random {seed}")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj stress with --gen-args");

    assert_eq!(
        ok_out.status.code(),
        Some(0),
        "uj stress with --gen-args failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&ok_out.stdout),
        String::from_utf8_lossy(&ok_out.stderr)
    );
    let ok_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&ok_out.stdout)).unwrap();
    assert_eq!(ok_json["ok"], true);
    assert_eq!(ok_json["iterations"], 5);

    let _ = fs::remove_dir_all(&test_dir);
}

#[test]
fn test_r12_pack_and_clean_orphans() {
    let _guard = lock_test();
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let test_dir = scratch_dir.join("test_r12_pack");
    let _ = fs::remove_dir_all(&test_dir);

    let _ = uj_cmd()
        .arg("init")
        .arg(&test_dir)
        .output()
        .expect("Failed to execute uj init");

    // Generate 3 tests and answers via pipeline
    let pipe_out = uj_cmd()
        .arg("pipeline")
        .arg("-p")
        .arg(&test_dir)
        .output()
        .expect("Failed to execute uj pipeline");
    assert_eq!(pipe_out.status.code(), Some(0));

    // Inject orphan files in tests/ (e.g. from an earlier larger generator count + stray .ans)
    fs::write(test_dir.join("tests/99.in"), "1 1\n").unwrap();
    fs::write(test_dir.join("tests/99.ans"), "2\n").unwrap();
    fs::write(test_dir.join("tests/orphan.ans"), "0\n").unwrap();

    let pack_out = uj_cmd()
        .arg("pack")
        .arg("-p")
        .arg(&test_dir)
        .arg("-o")
        .arg("data.zip")
        .arg("--clean")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj pack");

    assert_eq!(
        pack_out.status.code(),
        Some(0),
        "uj pack failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&pack_out.stdout),
        String::from_utf8_lossy(&pack_out.stderr)
    );

    let pack_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&pack_out.stdout)).unwrap();
    assert_eq!(pack_json["ok"], true);
    assert_eq!(pack_json["output"], "data.zip");
    assert_eq!(pack_json["test_count"], 3);
    assert!(pack_json["size_bytes"].as_u64().unwrap() > 0);

    let cleaned = pack_json["cleaned_files"].as_array().unwrap();
    assert_eq!(cleaned.len(), 3);
    assert!(!test_dir.join("tests/99.in").exists());
    assert!(!test_dir.join("tests/99.ans").exists());
    assert!(!test_dir.join("tests/orphan.ans").exists());
    assert!(test_dir.join("data.zip").is_file());

    let _ = fs::remove_dir_all(&test_dir);
}

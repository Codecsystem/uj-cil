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

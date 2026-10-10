use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn get_uj_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // pop test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(if cfg!(windows) { "uj.exe" } else { "uj" })
}

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

fn judge_evil_sol(sol_name: &str) -> serde_json::Value {
    let sol_path = format!("tests/evil/sol/{sol_name}");
    let output = uj_cmd()
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg("tests/evil")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute uj");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout_str).unwrap_or_else(|e| {
        panic!("Failed to parse JSON for {sol_name}: {e}. Output was: {stdout_str}\nStderr: {}", String::from_utf8_lossy(&output.stderr));
    })
}

#[test]
fn test_evil_ac_baseline() {
    let _guard = lock_test();
    let rep = judge_evil_sol("ac.cpp");
    assert_eq!(rep["verdict"], "AC");
    assert_eq!(rep["score"], 100.0);
}

#[test]
fn test_evil_cheat_read_blocked() {
    let _guard = lock_test();
    let rep = judge_evil_sol("cheat_read.cpp");
    // Standard answer file reading blocked by AppContainer -> prints -1 -> WA
    assert_eq!(rep["verdict"], "WA");
    assert_eq!(rep["score"], 0.0);
}

#[test]
fn test_evil_cheat_write_blocked() {
    let _guard = lock_test();
    let hacked_path = get_workspace_root().join("tests/evil/tests/HACKED.txt");
    let _ = fs::remove_file(&hacked_path);

    let rep = judge_evil_sol("cheat_write.cpp");
    assert_eq!(rep["verdict"], "AC");
    assert!(!hacked_path.exists(), "HACKED.txt must NOT be created in tests directory!");
}

#[test]
fn test_evil_write_cwd_no_leak() {
    let _guard = lock_test();
    let rep = judge_evil_sol("write_cwd.cpp");
    assert_eq!(rep["verdict"], "AC");

    // Verify leak.txt does not exist anywhere in tests/evil or workspace root
    let root = get_workspace_root();
    assert!(!root.join("leak.txt").exists());
    assert!(!root.join("tests/evil/leak.txt").exists());
}

#[test]
fn test_evil_crashes_and_re() {
    let _guard = lock_test();

    // abort() must be RE
    let rep_abort = judge_evil_sol("abort.cpp");
    assert_eq!(rep_abort["verdict"], "RE", "abort() must be judged as RE");

    // segv must be RE
    let rep_segv = judge_evil_sol("segv.cpp");
    assert_eq!(rep_segv["verdict"], "RE", "segv must be judged as RE");

    // divzero must be RE
    let rep_divzero = judge_evil_sol("divzero.cpp");
    assert_eq!(rep_divzero["verdict"], "RE", "divzero must be judged as RE");

    // ret1 must be RE
    let rep_ret1 = judge_evil_sol("ret1.cpp");
    assert_eq!(rep_ret1["verdict"], "RE", "ret1 must be judged as RE");

    // stack_overflow must be RE
    let rep_so = judge_evil_sol("stack_overflow.cpp");
    assert_eq!(rep_so["verdict"], "RE", "stack_overflow must be judged as RE");
}

#[test]
fn test_evil_deep_recursion_ac() {
    let _guard = lock_test();
    // 1e6 deep recursion under 256MB stack
    let rep = judge_evil_sol("deep_rec.cpp");
    assert_eq!(rep["verdict"], "AC");
}

#[test]
fn test_evil_ole() {
    let _guard = lock_test();
    let rep = judge_evil_sol("ole.cpp");
    assert_eq!(rep["verdict"], "OLE");
}

#[test]
fn test_evil_timeouts() {
    let _guard = lock_test();

    // Infinite busy loop -> TLE (Cpu)
    let rep_loop = judge_evil_sol("loop.cpp");
    assert_eq!(rep_loop["verdict"], "TLE");

    // Sleep -> TLE (Wall)
    let rep_sleep = judge_evil_sol("sleep.cpp");
    assert_eq!(rep_sleep["verdict"], "TLE");
}

#[test]
fn test_evil_batch_all_seeds() {
    let _guard = lock_test();

    // Run batch judging on all evil seed solutions
    let output = uj_cmd()
        .arg("judge")
        .arg("tests/evil/sol/*.cpp")
        .arg("-p")
        .arg("tests/evil")
        .arg("--format")
        .arg("json")
        .output()
        .expect("Failed to execute batch judge");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let arr: serde_json::Value = serde_json::from_str(&stdout_str).unwrap_or_else(|e| {
        panic!("Failed to parse batch JSON: {e}\nStdout: {stdout_str}\nStderr: {}", String::from_utf8_lossy(&output.stderr));
    });

    let items = arr.as_array().expect("Batch judging with multiple files must output a JSON array");
    assert!(items.len() >= 20, "Expected at least 20 evil solutions evaluated, got {}", items.len());

    // Ensure HACKED.txt was never written during batch judging
    let hacked_path = get_workspace_root().join("tests/evil/tests/HACKED.txt");
    assert!(!hacked_path.exists(), "HACKED.txt must NOT exist after batch evaluation");
}

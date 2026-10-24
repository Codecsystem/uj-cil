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
        panic!(
            "Failed to parse JSON for {sol_name}: {e}. Output was: {stdout_str}\nStderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    let in1_path = get_workspace_root().join("tests/evil/tests/1.in");
    let orig_in1 = fs::read_to_string(&in1_path).expect("read 1.in");

    let rep = judge_evil_sol("cheat_write.cpp");
    assert_eq!(rep["verdict"], "AC");
    assert!(
        !hacked_path.exists(),
        "HACKED.txt must NOT be created in tests directory!"
    );
    let after_in1 = fs::read_to_string(&in1_path).expect("read 1.in after");
    assert_eq!(
        orig_in1, after_in1,
        "1.in must NOT be modified via /proc/self/fd/0!"
    );
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

    // stack_overflow must be RE or MLE (under 256MB memory limit)
    let rep_so = judge_evil_sol("stack_overflow.cpp");
    assert!(
        rep_so["verdict"] == "RE" || rep_so["verdict"] == "MLE",
        "stack_overflow must be judged as RE or MLE, got {}",
        rep_so["verdict"]
    );
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
fn test_evil_mle() {
    let _guard = lock_test();

    // mle_touch: loops allocating and touching memory
    let rep_touch = judge_evil_sol("mle_touch.cpp");
    assert_eq!(
        rep_touch["verdict"], "MLE",
        "mle_touch must be judged as MLE"
    );

    // mle_vector: instant large chunk allocation exceeding limit
    let rep_vector = judge_evil_sol("mle_vector.cpp");
    assert_eq!(
        rep_vector["verdict"], "MLE",
        "mle_vector (vector resize 600MB) must be judged as MLE"
    );
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
        panic!(
            "Failed to parse batch JSON: {e}\nStdout: {stdout_str}\nStderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    let items = arr
        .as_array()
        .expect("Batch judging with multiple files must output a JSON array");
    assert!(
        items.len() >= 20,
        "Expected at least 20 evil solutions evaluated, got {}",
        items.len()
    );

    // Ensure HACKED.txt was never written during batch judging
    let hacked_path = get_workspace_root().join("tests/evil/tests/HACKED.txt");
    assert!(
        !hacked_path.exists(),
        "HACKED.txt must NOT exist after batch evaluation"
    );
}

#[test]
fn test_evil_linux_lx_all() {
    let _guard = lock_test();

    #[cfg(target_os = "linux")]
    {
        let rep = judge_evil_sol("lx_all.cpp");
        assert_eq!(rep["verdict"], "WA");

        let test0 = &rep["tests"][0];
        let found = test0["diff"]["found"]
            .as_str()
            .expect("diff.found should be present");

        // Parse key-value tokens from probe string (e.g., A=-1,B=-1,C=0,D=1,E=0,F=1,G=0,pid=1,ppid=0,pd=)
        let mut map = std::collections::HashMap::new();
        for part in found.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.trim(), v.trim());
            }
        }

        let c: i32 = map.get("C").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let e: i32 = map.get("E").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let g: i32 = map.get("G").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let f: i32 = map.get("F").and_then(|v| v.parse().ok()).unwrap_or(-99);

        assert_eq!(
            c, 0,
            "C == 0: Writing to problem directory / rootfs must fail"
        );
        assert_eq!(e, 0, "E == 0: Writing to /home must fail");
        assert_eq!(g, 0, "G == 0: Reading sensitive host files must fail");
        assert!(
            f <= 3,
            "F <= 3: Visible /proc numeric entries must not exceed 3, got {f}"
        );

        if test0["sandbox_tier"] == "full" {
            let pid: i32 = map.get("pid").and_then(|v| v.parse().ok()).unwrap_or(-99);
            assert!(
                pid == 1 || pid == 2,
                "Child must be PID 1 or PID 2 in new PID namespace, got {pid}"
            );
        }

        // Assert no escape files were created on the host
        let root = get_workspace_root();
        assert!(!root.join("tests/evil/PWNED_lx.txt").exists());
        assert!(!root.join("PWNED_lx.txt").exists());
        assert!(!std::path::Path::new("/tmp/uj_pwned_lx.txt").exists());
        assert!(!std::path::Path::new("/tmp/uj_lx_report.txt").exists());
    }
}

#[test]
fn test_evil_linux_seccomp() {
    let _guard = lock_test();

    #[cfg(target_os = "linux")]
    {
        let rep = judge_evil_sol("lx_seccomp.cpp");
        assert_eq!(rep["verdict"], "WA");

        let test0 = &rep["tests"][0];
        let found = test0["diff"]["found"]
            .as_str()
            .expect("diff.found should be present");

        // Format: unshare=-1/1,ptrace=-1/1,x32mount=-1/38,ot=-1/38,ms=-1/38,iu=-1/38,u=1000,p1=-1,srw=-1
        let mut map = std::collections::HashMap::new();
        for part in found.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.trim(), v.trim());
            }
        }

        if test0["sandbox_tier"] == "full" {
            // unshare must fail with EPERM (1) or ENOSYS (38)
            let unshare = map.get("unshare").copied().unwrap_or("");
            assert!(
                unshare == "-1/1" || unshare == "-1/38",
                "unshare must be blocked with EPERM(1) or ENOSYS(38), got: {unshare}"
            );

            // ptrace must fail with EPERM (1) or ENOSYS (38)
            let ptrace = map.get("ptrace").copied().unwrap_or("");
            assert!(
                ptrace == "-1/1" || ptrace == "-1/38",
                "ptrace must be blocked with EPERM(1) or ENOSYS(38), got: {ptrace}"
            );

            // x32mount must fail with ENOSYS (38) or EPERM (1) and NEVER EFAULT (14)
            let x32mount = map.get("x32mount").copied().unwrap_or("");
            assert_ne!(
                x32mount, "-1/14",
                "x32 mount must not return EFAULT(14) (indicates x32 seccomp filter bypass)"
            );
            assert!(
                x32mount == "-1/38" || x32mount == "-1/1",
                "x32 mount must be blocked with ENOSYS(38) or EPERM(1), got: {x32mount}"
            );

            // New mount API & io_uring syscalls must fail with EPERM (1) or ENOSYS (38)
            for key in ["ot", "ms", "iu"] {
                let val = map.get(key).copied().unwrap_or("");
                assert!(
                    val == "-1/1" || val == "-1/38",
                    "{key} must be blocked with EPERM(1) or ENOSYS(38), got: {val}"
                );
            }

            // UID must be non-zero inside the sandbox
            let uid = map.get("u").copied().unwrap_or("0");
            assert_ne!(uid, "0", "UID inside sandbox must not be 0 (root)");

            // PID 1 must be non-dumpable so /proc/1/fd/3 cannot be opened
            let pid1fd = map.get("p1").copied().unwrap_or("0");
            assert_eq!(pid1fd, "-1", "Opening /proc/1/fd/3 must fail");

            // Re-opening /proc/self/fd/0 for writing must fail
            let stdinrw = map.get("srw").copied().unwrap_or("0");
            assert_eq!(stdinrw, "-1", "Re-opening /proc/self/fd/0 O_RDWR must fail");
        }
    }
}

#[test]
fn test_evil_windows_win_probe() {
    let _guard = lock_test();

    #[cfg(windows)]
    {
        let rep = judge_evil_sol("win_probe.cpp");
        assert_eq!(rep["verdict"], "WA");

        let test0 = &rep["tests"][0];
        let found = test0["diff"]["found"]
            .as_str()
            .expect("diff.found should be present");

        // Format: A=-1,C=0,E=0,G=0,T=0,K=1,pd=...
        let mut map = std::collections::HashMap::new();
        for part in found.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.trim(), v.trim());
            }
        }

        let a: i32 = map.get("A").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let c: i32 = map.get("C").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let e: i32 = map.get("E").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let g: i32 = map.get("G").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let t: i32 = map.get("T").and_then(|v| v.parse().ok()).unwrap_or(-99);
        let k: i32 = map.get("K").and_then(|v| v.parse().ok()).unwrap_or(-99);

        if test0["sandbox_tier"] == "full" {
            assert_eq!(a, -1, "A == -1: Reading answer file must be blocked");
            assert_eq!(c, 0, "C == 0: Writing to problem directory must fail");
            assert_eq!(e, 0, "E == 0: Writing to user profile must fail");
            assert_eq!(g, 0, "G == 0: Reading user profile .gitconfig must fail");
            assert_eq!(t, 0, "T == 0: Writing to temp directory must fail");
            assert_eq!(k, 1, "K == 1: Writing to own box directory MUST succeed");
        }

        let root = get_workspace_root();
        assert!(!root.join("tests/evil/PWNED_win.txt").exists());
        assert!(!root.join("PWNED_win.txt").exists());
    }
}

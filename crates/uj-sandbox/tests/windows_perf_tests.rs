//! Round 14 Windows sandbox performance / correctness regression tests.
#![cfg(windows)]

use std::ffi::OsString;
use uj_core::run::{KillReason, Limits, RunSpec, Stdio};
use uj_sandbox::windows::security::AppContainerProfile;
use uj_sandbox::{CancelToken, create_sandbox};

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static CNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = uj_sandbox::get_sandbox_scratch_dir()
        .join(format!("uj_r14_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Micro benchmark: per-slot ACL grant + revoke on a fresh box directory.
/// Run with `cargo test -p uj-sandbox --test windows_perf_tests -- --ignored --nocapture bench_acl`.
#[test]
#[ignore]
fn bench_acl_grant_revoke() {
    let profile = match AppContainerProfile::create_isolated() {
        Ok(p) => p,
        Err(_) => return,
    };
    let sid = profile.sid_string().unwrap();
    let base = unique_dir("bench_acl");
    let iters = 50u32;
    let mut grant_total = std::time::Duration::ZERO;
    let mut revoke_total = std::time::Duration::ZERO;
    for i in 0..iters {
        let d = base.join(format!("box{i}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("input.txt"), b"1 2\n").unwrap();
        let t = std::time::Instant::now();
        AppContainerProfile::grant_path_access(&d, &sid).unwrap();
        grant_total += t.elapsed();
        let t = std::time::Instant::now();
        AppContainerProfile::revoke_path_access(&d, &sid).unwrap();
        revoke_total += t.elapsed();
    }
    let exe = base.join("prog.exe");
    std::fs::write(&exe, b"MZ").unwrap();
    let t = std::time::Instant::now();
    for _ in 0..iters {
        AppContainerProfile::grant_path_read_execute(&exe, &sid).unwrap();
    }
    let rx_total = t.elapsed();
    println!(
        "ACL bench ({iters} iters): grant(OI|CI F) avg = {:.3} ms, revoke avg = {:.3} ms, grant RX avg = {:.3} ms",
        grant_total.as_secs_f64() * 1000.0 / iters as f64,
        revoke_total.as_secs_f64() * 1000.0 / iters as f64,
        rx_total.as_secs_f64() * 1000.0 / iters as f64,
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Micro benchmark: average wall overhead of a trivial native binary under the Full tier.
#[test]
#[ignore]
fn bench_trivial_run_overhead() {
    let sandbox = match create_sandbox("full", true) {
        Ok(s) => s,
        Err(_) => return,
    };
    let base = std::env::current_dir()
        .unwrap()
        .join(".uj")
        .join("tmp")
        .join(format!("r14_bench_run_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let box_dir = sandbox.create_box_dir(&base).unwrap();
    let rs_path = base.join("noop.rs");
    let exe_path = box_dir.join("noop.exe");
    std::fs::write(&rs_path, b"fn main() {}\n").unwrap();
    let compiled = std::process::Command::new("rustc")
        .arg(&rs_path)
        .arg("-O")
        .arg("-o")
        .arg(&exe_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !compiled {
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    sandbox.authorize_executable(&exe_path).unwrap();
    let spec = RunSpec {
        argv: vec![exe_path.into_os_string()],
        env: vec![],
        work_dir: box_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(5000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };
    // Warmup run (absorbs Windows Defender first-launch scan)
    let warm = sandbox.run(&spec, &CancelToken::new()).unwrap();
    assert!(warm.status.is_success());

    let iters = 30u32;
    let t = std::time::Instant::now();
    for _ in 0..iters {
        let res = sandbox.run(&spec, &CancelToken::new()).unwrap();
        assert!(res.status.is_success());
    }
    println!(
        "Windows Full AppContainer trivial run avg ({iters} iters) = {:.3} ms",
        t.elapsed().as_secs_f64() * 1000.0 / iters as f64
    );
    drop(sandbox);
    let _ = std::fs::remove_dir_all(&base);
}

/// memory_bytes == 0 means "unlimited" and must never produce an instant MLE.
#[test]
fn test_memory_limit_zero_is_unlimited() {
    for mode in ["degraded", "none"] {
        let sandbox = create_sandbox(mode, false).expect("sandbox");
        let work_dir = unique_dir("mem0");
        let mut limits = Limits::new(5000, 0);
        limits.memory_bytes = 0;
        let spec = RunSpec {
            argv: vec![
                OsString::from("cmd.exe"),
                OsString::from("/c"),
                OsString::from("echo ok"),
            ],
            env: vec![],
            work_dir: work_dir.clone(),
            ro_binds: vec![],
            stdin: Stdio::Null,
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits,
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: false,
        };
        let res = sandbox.run(&spec, &CancelToken::new()).unwrap();
        assert!(res.status.is_success(), "{mode}: status={:?}", res.status);
        assert_eq!(res.killed_by, None, "{mode}: unexpected kill");
        assert!(!res.oom_killed, "{mode}: memory_bytes=0 must not be MLE");

        // Group path as well.
        let results = sandbox
            .run_group(
                std::slice::from_ref(&spec),
                &uj_sandbox::Wiring::default(),
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(results[0].killed_by, None, "{mode}: group unexpected kill");
        assert!(
            !results[0].oom_killed,
            "{mode}: group memory_bytes=0 must not be MLE"
        );
        let _ = std::fs::remove_dir_all(&work_dir);
    }
}

/// Query total CPU time (user + kernel) consumed by the current (judge) process, in ms.
fn self_cpu_ms() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (f(k) + f(u)) / 10_000
}

/// When the main process exits while a descendant is still alive (max_processes > 1),
/// the monitor must block on the job instead of spinning at 100% CPU until the wall limit.
#[test]
fn test_descendant_alive_does_not_spin() {
    let sandbox = create_sandbox("degraded", false).expect("sandbox");
    let work_dir = unique_dir("spin");
    let mut limits = Limits::new(10_000, 256 * 1024 * 1024);
    limits.wall_time_ms = 1500;
    limits.max_processes = 8;
    // `start /b` detaches a child (ping ~3s) that outlives cmd.exe.
    let spec = RunSpec {
        argv: vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("start /b ping -n 4 127.0.0.1 >nul"),
        ],
        env: vec![],
        work_dir: work_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };
    let cpu_before = self_cpu_ms();
    let t = std::time::Instant::now();
    let res = sandbox.run(&spec, &CancelToken::new()).unwrap();
    let wall = t.elapsed();
    let cpu_used = self_cpu_ms().saturating_sub(cpu_before);
    println!(
        "descendant-alive run: killed_by={:?} wall={:?} judge_cpu={}ms",
        res.killed_by, wall, cpu_used
    );
    // The descendant keeps the job alive past the wall limit.
    assert_eq!(res.killed_by, Some(KillReason::WallLimit));
    // A busy-spinning monitor would burn ~1500 ms of CPU; allow generous slack for
    // parallel test threads (this counter is process-wide).
    assert!(
        cpu_used < 1000,
        "monitor appears to busy-spin: judge process used {cpu_used} ms CPU in {wall:?}"
    );
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// Group runs must report exit order and per-process wall time accurately.
#[test]
fn test_group_exit_order_and_wall_accuracy() {
    let sandbox = create_sandbox("degraded", false).expect("sandbox");
    let work_dir = unique_dir("grp");
    let mk = |argv: Vec<&str>| RunSpec {
        argv: argv.into_iter().map(OsString::from).collect(),
        env: vec![],
        work_dir: work_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(5000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };
    // Single-process programs only (default max_processes forbids children).
    let slow = mk(vec![
        "powershell.exe",
        "-NoProfile",
        "-Command",
        "Start-Sleep -Milliseconds 400",
    ]);
    let fast = mk(vec!["cmd.exe", "/c", "exit 0"]);
    let results = sandbox
        .run_group(
            &[slow, fast],
            &uj_sandbox::Wiring::default(),
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(results[1].exit_order, Some(1));
    assert_eq!(results[0].exit_order, Some(2));
    assert!(results[1].wall_time_us < results[0].wall_time_us);
    let _ = std::fs::remove_dir_all(&work_dir);
}

fn icacls_show(path: &std::path::Path) -> String {
    let out = std::process::Command::new("icacls.exe")
        .arg(path)
        .output()
        .expect("icacls");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The in-process ACL path must produce the same DACL shape as the former
/// `icacls <dir> /grant *SID:(OI)(CI)F /T` / `icacls <exe> /grant *SID:RX` / `/remove` calls.
#[test]
fn test_native_acl_semantics() {
    let profile = match AppContainerProfile::create_isolated() {
        Ok(p) => p,
        Err(_) => return,
    };
    let sid = profile.sid_string().unwrap();
    let base = unique_dir("acl_sem");
    let box_dir = base.join("box");
    let sub = box_dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let child = sub.join("in.txt");
    std::fs::write(&child, b"x").unwrap();
    // A pre-existing child with a protected DACL (inheritance disabled) must still be granted.
    let protected = box_dir.join("protected.txt");
    std::fs::write(&protected, b"p").unwrap();
    let st = std::process::Command::new("icacls.exe")
        .arg(&protected)
        .args(["/inheritance:d", "/Q"])
        .status()
        .unwrap();
    assert!(st.success());
    let sibling = base.join("sibling.txt");
    std::fs::write(&sibling, b"s").unwrap();

    profile.grant_access_to_path(&box_dir).unwrap();

    let root = icacls_show(&box_dir);
    assert!(
        root.contains(&format!("{sid}:(OI)(CI)(F)")),
        "root must carry an explicit inheritable full-control ACE: {root}"
    );
    let c = icacls_show(&child);
    assert!(
        c.contains(&format!("{sid}:(I)(F)")),
        "existing child must inherit the ACE: {c}"
    );
    let p = icacls_show(&protected);
    assert!(
        p.contains(&format!("{sid}:(F)")),
        "protected child must receive an explicit ACE: {p}"
    );
    let s = icacls_show(&sibling);
    assert!(
        !s.contains(&sid),
        "sibling outside the box must not be granted: {s}"
    );
    assert!(
        !icacls_show(&base).contains(&sid),
        "parent must not be granted"
    );

    // Executable RX grant (no inheritance flags).
    let exe = base.join("prog.exe");
    std::fs::write(&exe, b"MZ").unwrap();
    profile.grant_read_execute(&exe).unwrap();
    let e = icacls_show(&exe);
    assert!(e.contains(&format!("{sid}:(RX)")), "exe must be RX: {e}");

    // Revocation removes the explicit ACE and the inherited copies.
    profile.revoke_access(&box_dir).unwrap();
    profile.revoke_access(&exe).unwrap();
    assert!(!icacls_show(&box_dir).contains(&sid));
    assert!(!icacls_show(&child).contains(&sid));
    assert!(!icacls_show(&exe).contains(&sid));

    // Problem directories are refused (Fail-Closed).
    let prob = base.join("prob");
    std::fs::create_dir_all(&prob).unwrap();
    std::fs::write(prob.join("problem.toml"), b"").unwrap();
    assert!(profile.grant_access_to_path(&prob).is_err());
    assert!(AppContainerProfile::grant_path_access(&prob, &sid).is_err());
    // Invalid SID strings are rejected rather than silently ignored.
    assert!(AppContainerProfile::grant_path_access(&box_dir, "not-a-sid").is_err());

    let _ = std::fs::remove_dir_all(&base);
}

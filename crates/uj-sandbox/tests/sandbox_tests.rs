use std::ffi::OsString;
use uj_core::run::{KillReason, Limits, RunSpec, Stdio};
use uj_sandbox::{CancelToken, create_sandbox};

#[test]
fn test_windows_sandbox_echo() {
    let sandbox = create_sandbox("none", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir();
    let spec = RunSpec {
        argv: vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo hello_uj_sandbox"),
        ],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(2000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let cancel = CancelToken::new();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");

    assert!(res.status.is_success());
    assert!(res.killed_by.is_none());
    let stdout_captured = res.stdout.expect("stdout should be captured");
    let text = String::from_utf8_lossy(&stdout_captured.bytes);
    assert!(text.contains("hello_uj_sandbox"));
}

#[test]
fn test_windows_appcontainer_user_binary() {
    let sandbox = match create_sandbox("full", true) {
        Ok(s) => s,
        Err(_) => return, // Skip if AppContainer cannot be created (e.g. non-admin CI)
    };

    let work_dir = std::path::PathBuf::from(r"C:\ProgramData\uj_sandbox_test");
    let _ = std::fs::create_dir_all(&work_dir);
    let exe = work_dir.join("test_prog.exe");
    if !exe.exists() {
        let rs_path = work_dir.join("test.rs");
        let _ = std::fs::write(&rs_path, b"fn main() { println!(\"HELLO_FROM_AC\"); }");
        let compile_ok = std::process::Command::new("rustc")
            .arg(&rs_path)
            .arg("-o")
            .arg(&exe)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !compile_ok {
            return;
        }
        let _ = uj_sandbox::windows::security::AppContainerProfile::grant_all_application_packages(
            &work_dir,
        );
    }

    let spec = RunSpec {
        argv: vec![exe.into_os_string()],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(5000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let cancel = CancelToken::new();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");

    println!(
        "AppContainer run result: status={:?}, killed_by={:?}",
        res.status, res.killed_by
    );
    assert!(res.status.is_success());
    let stdout_captured = res.stdout.expect("stdout should be captured");
    let text = String::from_utf8_lossy(&stdout_captured.bytes);
    assert!(text.contains("HELLO_FROM_AC"));
}

#[test]
fn test_windows_sandbox_wall_time_limit() {
    let sandbox = create_sandbox("degraded", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir().join("uj_ac_test");
    let mut limits = Limits::new(5000, 256 * 1024 * 1024);
    limits.wall_time_ms = 150; // Very short wall time

    let spec = RunSpec {
        argv: vec![
            OsString::from("powershell.exe"),
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from("Start-Sleep -Seconds 5"),
        ],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let cancel = CancelToken::new();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");
    println!(
        "Status: {:?}, Stderr: {}",
        res.status,
        String::from_utf8_lossy(&res.stderr_tail)
    );

    assert_eq!(res.killed_by, Some(KillReason::WallLimit));
}

#[test]
fn test_windows_sandbox_output_limit() {
    let sandbox = create_sandbox("auto", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir();
    let mut limits = Limits::new(5000, 256 * 1024 * 1024);
    limits.output_bytes = 100; // Small limit: 100 bytes

    let spec = RunSpec {
        argv: vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            // Echo a string longer than 100 bytes
            OsString::from(
                "echo 12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890",
            ),
        ],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let cancel = CancelToken::new();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");
    println!(
        "Output Limit Test: status={:?}, killed_by={:?}, stdout len={:?}, stderr={}",
        res.status,
        res.killed_by,
        res.stdout.as_ref().map(|o| o.bytes.len()),
        String::from_utf8_lossy(&res.stderr_tail)
    );

    assert_eq!(res.killed_by, Some(KillReason::OutputLimit));
    assert!(res.stdout.unwrap().truncated);
}

#[test]
fn test_windows_sandbox_cancel_token() {
    let sandbox = create_sandbox("degraded", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir();
    let spec = RunSpec {
        argv: vec![
            OsString::from("powershell.exe"),
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from("Start-Sleep -Seconds 10"),
        ],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(10000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let cancel = CancelToken::new();
    let cancel_clone = cancel.clone();

    // Cancel after 100ms
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        cancel_clone.cancel();
    });

    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");
    assert_eq!(res.killed_by, Some(KillReason::UserCancel));
}

#[test]
fn test_windows_run_group_wiring() {
    use uj_sandbox::{PipeConnection, Wiring};

    let sandbox = create_sandbox("auto", false).expect("failed to create sandbox");
    let work_dir = uj_sandbox::get_sandbox_scratch_dir();

    let spec_sender = RunSpec {
        argv: vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo WIRE_TOKEN_12345"),
        ],
        env: vec![],
        work_dir: work_dir.clone(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(5000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let spec_receiver = RunSpec {
        argv: vec![OsString::from("findstr.exe"), OsString::from("WIRE_TOKEN")],
        env: vec![],
        work_dir,
        ro_binds: vec![],
        stdin: Stdio::Pipe,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(5000, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
    };

    let wiring = Wiring {
        connections: vec![PipeConnection {
            from_proc: 0,
            to_proc: 1,
        }],
    };

    let cancel = CancelToken::new();
    let results = sandbox
        .run_group(&[spec_sender, spec_receiver], &wiring, &cancel)
        .expect("run_group failed");

    assert_eq!(results.len(), 2);
    assert!(results[0].status.is_success());
    assert!(results[1].status.is_success());

    let recv_out = results[1].stdout.as_ref().expect("receiver stdout missing");
    let text = String::from_utf8_lossy(&recv_out.bytes);
    assert!(
        text.contains("WIRE_TOKEN_12345"),
        "Expected receiver to receive piped token, got: {text}"
    );
}

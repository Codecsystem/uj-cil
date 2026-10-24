use std::ffi::OsString;
use uj_core::run::{KillReason, Limits, RunSpec, Stdio};
use uj_sandbox::{CancelToken, create_sandbox};

#[test]
fn test_windows_sandbox_echo() {
    let sandbox = create_sandbox("none", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir();
    let argv = if cfg!(windows) {
        vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo hello_uj_sandbox"),
        ]
    } else {
        vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("echo hello_uj_sandbox"),
        ]
    };
    let spec = RunSpec {
        argv,
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
        trusted: false,
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

    let work_dir = uj_sandbox::create_isolated_box_dir(std::path::Path::new("."))
        .unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir());
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
        trusted: false,
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

#[cfg(windows)]
#[test]
fn test_windows_appcontainer_stack_overflow() {
    let sandbox = match create_sandbox("full", false) {
        Ok(s) => s,
        Err(_) => return,
    };

    let work_dir = uj_sandbox::create_isolated_box_dir(std::path::Path::new("."))
        .unwrap_or_else(|_| uj_sandbox::get_sandbox_scratch_dir());
    let cpp_path = work_dir.join("so.cpp");
    let exe = work_dir.join("prog.exe");
    std::fs::write(&cpp_path, b"int f(int n){volatile char b[4096]; b[0]=n; return f(n+1)+b[0];} int main(){return f(0);}").unwrap();
    let compiled = std::process::Command::new("g++")
        .args(["-O2", "-Wl,--stack,268435456", "-o"])
        .arg(&exe)
        .arg(&cpp_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !compiled {
        return;
    }
    uj_sandbox::prewarm_binary(&exe, &work_dir);

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
        trusted: false,
    };

    let cancel = CancelToken::new();
    let start = std::time::Instant::now();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");
    let elapsed = start.elapsed();
    println!(
        "SO AppContainer run result: status={:?}, killed_by={:?}, wall_ms={}, elapsed={:?}",
        res.status,
        res.killed_by,
        res.wall_time_us / 1000,
        elapsed
    );
}

#[test]
fn test_windows_sandbox_wall_time_limit() {
    let sandbox = create_sandbox("degraded", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir().join("uj_ac_test");
    let _ = std::fs::create_dir_all(&work_dir);
    let mut limits = Limits::new(5000, 256 * 1024 * 1024);
    limits.wall_time_ms = 150; // Very short wall time

    let argv = if cfg!(windows) {
        vec![
            OsString::from("powershell.exe"),
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from("Start-Sleep -Seconds 5"),
        ]
    } else {
        vec![OsString::from("sleep"), OsString::from("5")]
    };

    let spec = RunSpec {
        argv,
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
        trusted: false,
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

    let work_dir = std::env::temp_dir().join("uj_test_out");
    let _ = std::fs::create_dir_all(&work_dir);
    let mut limits = Limits::new(5000, 256 * 1024 * 1024);
    limits.output_bytes = 100; // Small limit: 100 bytes

    let argv = if cfg!(windows) {
        vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            // Echo a string longer than 100 bytes
            OsString::from(
                "echo 12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890",
            ),
        ]
    } else {
        vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from(
                "echo 12345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890",
            ),
        ]
    };

    let spec = RunSpec {
        argv,
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
        trusted: false,
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

    let work_dir = std::env::temp_dir().join("uj_test_cancel");
    let _ = std::fs::create_dir_all(&work_dir);
    let argv = if cfg!(windows) {
        vec![
            OsString::from("powershell.exe"),
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from("Start-Sleep -Seconds 10"),
        ]
    } else {
        vec![OsString::from("sleep"), OsString::from("10")]
    };

    let spec = RunSpec {
        argv,
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
        trusted: false,
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

    let argv_sender = if cfg!(windows) {
        vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo WIRE_TOKEN_12345"),
        ]
    } else {
        vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("echo WIRE_TOKEN_12345"),
        ]
    };

    let spec_sender = RunSpec {
        argv: argv_sender,
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
        trusted: false,
    };

    let argv_receiver = if cfg!(windows) {
        vec![OsString::from("findstr.exe"), OsString::from("WIRE_TOKEN")]
    } else {
        vec![OsString::from("grep"), OsString::from("WIRE_TOKEN")]
    };

    let spec_receiver = RunSpec {
        argv: argv_receiver,
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
        trusted: false,
    };

    let wiring = Wiring {
        connections: vec![PipeConnection {
            from_proc: 0,
            to_proc: 1,
        }],
        star_manager_proc: None,
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

#[test]
fn test_windows_sandbox_memory_limit() {
    let sandbox = create_sandbox("degraded", false).expect("failed to create sandbox");

    let work_dir = std::env::temp_dir().join("uj_test_mem");
    let _ = std::fs::create_dir_all(&work_dir);
    let limits = Limits::new(5000, 32 * 1024 * 1024);

    let argv = if cfg!(windows) {
        vec![
            OsString::from("powershell.exe"),
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from("[byte[]]::new(100*1024*1024)"),
        ]
    } else {
        vec![
            OsString::from("python3"),
            OsString::from("-c"),
            OsString::from("b = bytearray(100*1024*1024)"),
        ]
    };

    let spec = RunSpec {
        argv,
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
        trusted: false,
    };

    let cancel = CancelToken::new();
    let res = sandbox.run(&spec, &cancel).expect("sandbox run failed");
    println!(
        "Memory Limit Test: status={:?}, killed_by={:?}, peak_mem_mb={}",
        res.status,
        res.killed_by,
        res.memory_peak_bytes / (1024 * 1024)
    );
}

/// 多个线程以同一个 work_dir 并发运行非可信程序，不得出现 Exited(127)，
/// 且 work_dir 中不得残留沙盒内部的挂载点文件。
#[cfg(target_os = "linux")]
#[test]
fn test_concurrent_untrusted_runs_share_work_dir() {
    let dir = tempfile::tempdir().unwrap();
    // 可执行文件必须位于 /usr、/bin 之外，才会走 bind 挂载路径
    let prog = dir.path().join("prog");
    std::fs::copy("/usr/bin/true", &prog).unwrap();

    let probe = uj_sandbox::create_sandbox("auto", false).unwrap();
    if probe.tier() == uj_core::run::Tier::None {
        eprintln!("skip: sandbox tier None, rootfs path not exercised");
        return;
    }

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let prog = prog.clone();
            let work_dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let sb = uj_sandbox::create_sandbox("auto", false).unwrap();
                let cancel = uj_sandbox::CancelToken::new();
                for _ in 0..20 {
                    let spec = RunSpec {
                        argv: vec![prog.clone().into_os_string()],
                        env: vec![],
                        work_dir: work_dir.clone(),
                        ro_binds: vec![],
                        stdin: Stdio::Null,
                        stdout: Stdio::Null,
                        stderr: Stdio::Null,
                        extra_fds: vec![],
                        limits: Limits::new(1000, 256 * 1024 * 1024),
                        cpu_affinity: None,
                        seccomp_profile: Default::default(),
                        trusted: false,
                    };
                    let res = sb.run(&spec, &cancel).unwrap();
                    assert!(
                        res.status.is_success(),
                        "untrusted run failed: {:?}",
                        res.status
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(!dir.path().join("__uj_bin").exists());
}

/// Item 1: Untrusted Linux sandbox runs with a non-zero UID and zero capabilities (`CapEff == 0`).
#[cfg(target_os = "linux")]
#[test]
fn test_linux_untrusted_zero_caps_and_non_root_uid() {
    let sb = create_sandbox("auto", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let spec = RunSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("id -u && grep '^CapEff:' /proc/self/status"),
        ],
        env: vec![],
        work_dir: dir.path().to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(2000, 128 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };
    let res = sb.run(&spec, &cancel).unwrap();
    assert!(
        res.status.is_success(),
        "status={:?}, stderr={}",
        res.status,
        String::from_utf8_lossy(&res.stderr_tail)
    );
    let out = String::from_utf8_lossy(&res.stdout.unwrap().bytes).into_owned();
    let mut lines = out.lines();
    let uid_line = lines.next().unwrap_or("").trim();
    let cap_line = lines.next().unwrap_or("").trim();
    assert_ne!(
        uid_line, "0",
        "Contestant must not run as UID 0 inside userns"
    );
    assert!(
        cap_line.ends_with("0000000000000000"),
        "Contestant must have zero effective capabilities, got: {cap_line}"
    );
}

/// Item 2: PID 1 `/proc/1/*` is non-dumpable (root-owned) so PID 2 cannot access `/proc/1/fd/*` or `/proc/1/environ`
/// to spoof `SandboxStatusPacket` or read host environment secrets.
#[cfg(target_os = "linux")]
#[test]
fn test_linux_pid1_proc_protected_and_status_pipe_unspoofable() {
    let sb = create_sandbox("auto", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let spec = RunSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(
                "if cat /proc/1/environ >/dev/null 2>&1 || echo spoof > /proc/1/fd/3 2>/dev/null; then exit 0; fi; kill -SEGV $$",
            ),
        ],
        env: vec![],
        work_dir: dir.path().to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(2000, 128 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };
    let res = sb.run(&spec, &cancel).unwrap();
    assert_eq!(
        res.status,
        uj_core::run::ExitStatus::Signaled(libc::SIGSEGV),
        "PID 1 status pipe must report genuine SIGSEGV and block /proc/1 access"
    );
}

/// Item 3: `Stdio::File` stdin cannot be modified or chmodded via `/proc/self/fd/0`.
#[test]
fn test_stdin_file_cannot_be_corrupted() {
    let sb = create_sandbox("auto", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let input_path = dir.path().join("test_input.in");
    let original = b"SECRET_IMMUTABLE_TEST_INPUT_42\n";
    std::fs::write(&input_path, original).unwrap();

    let box_dir = dir.path().join("box");
    std::fs::create_dir_all(&box_dir).unwrap();

    let argv = if cfg!(windows) {
        vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo ok"),
        ]
    } else {
        vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(
                "chmod 666 /proc/self/fd/0 2>/dev/null; echo CORRUPTED > /proc/self/fd/0 2>/dev/null; cat",
            ),
        ]
    };

    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir: box_dir,
        ro_binds: vec![],
        stdin: Stdio::File(input_path.clone()),
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(2000, 128 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let cancel = CancelToken::new();
    let _ = sb.run(&spec, &cancel).unwrap();

    let after = std::fs::read(&input_path).unwrap();
    assert_eq!(
        after.as_slice(),
        original,
        "Host input file must remain completely unmodified"
    );
}

/// Item 5: `RLIMIT_FSIZE` is enforced in both Full and Degraded tiers on Linux so `/box` writes are capped.
#[cfg(target_os = "linux")]
#[test]
fn test_linux_rlimit_fsize_caps_box_file_writes() {
    let sb = create_sandbox("auto", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut limits = Limits::new(2000, 128 * 1024 * 1024);
    limits.output_bytes = 64 * 1024; // 64 KiB cap

    let spec = RunSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("dd if=/dev/zero of=/box/huge.bin bs=4096 count=256"),
        ],
        env: vec![],
        work_dir: dir.path().to_path_buf(),
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

    let cancel = CancelToken::new();
    let res = sb.run(&spec, &cancel).unwrap();
    let huge_path = dir.path().join("huge.bin");
    let written_size = std::fs::metadata(&huge_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        written_size <= 64 * 1024,
        "File in /box exceeded output_bytes limit: {written_size} > 65536"
    );
    assert!(
        !res.status.is_success() || res.killed_by == Some(KillReason::OutputLimit),
        "Expected dd exceeding RLIMIT_FSIZE to fail or be killed by OutputLimit"
    );
}

/// Item 6: Windows per-slot AppContainer isolates sibling directories (`trusted_int_*` and other slots).
#[cfg(windows)]
#[test]
fn test_windows_appcontainer_parent_and_cross_slot_isolation() {
    let sb1 = match create_sandbox("full", true) {
        Ok(s) => s,
        Err(_) => return,
    };
    let sb2 = match create_sandbox("full", true) {
        Ok(s) => s,
        Err(_) => return,
    };

    let base_dir = std::env::temp_dir().join(format!("uj_test_ac_iso_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();

    let box1 = sb1.create_box_dir(&base_dir).unwrap();
    let box2 = sb2.create_box_dir(&base_dir).unwrap();

    // Create a sibling trusted_int directory next to box1 (inside box1.parent())
    let trusted_int = box1.parent().unwrap().join("trusted_int_secret");
    std::fs::create_dir_all(&trusted_int).unwrap();
    let secret_int_file = trusted_int.join("interactor.xml");
    std::fs::write(&secret_int_file, b"TOP_SECRET_XML").unwrap();

    // Create a file in slot 2's box2
    let secret_slot2_file = box2.join("slot2_secret.txt");
    std::fs::write(&secret_slot2_file, b"SLOT2_SECRET").unwrap();

    let cmd_str = format!(
        "type \"{}\" 2>nul & type \"{}\" 2>nul",
        secret_int_file.display(),
        secret_slot2_file.display()
    );
    let spec = RunSpec {
        argv: vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from(cmd_str),
        ],
        env: vec![],
        work_dir: box1,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(3000, 128 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: false,
    };

    let cancel = CancelToken::new();
    let res = sb1.run(&spec, &cancel).unwrap();
    let out = String::from_utf8_lossy(&res.stdout.unwrap().bytes).into_owned();
    assert!(
        !out.contains("TOP_SECRET_XML"),
        "Slot 1 AppContainer must not be able to read trusted_int sibling directory!"
    );
    assert!(
        !out.contains("SLOT2_SECRET"),
        "Slot 1 AppContainer must not be able to read Slot 2's box directory!"
    );

    drop(sb1);
    drop(sb2);
    let _ = std::fs::remove_dir_all(&base_dir);
}

/// Item 7: Linux CPU affinity is applied via `sched_setaffinity` even in Degraded tier.
#[cfg(target_os = "linux")]
#[test]
fn test_linux_cpu_affinity_applied() {
    let sb = create_sandbox("degraded", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let spec = RunSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("grep '^Cpus_allowed_list:' /proc/self/status"),
        ],
        env: vec![],
        work_dir: dir.path().to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(2000, 128 * 1024 * 1024),
        cpu_affinity: Some(vec![0]),
        seccomp_profile: Default::default(),
        trusted: false,
    };
    let cancel = CancelToken::new();
    let res = sb.run(&spec, &cancel).unwrap();
    assert!(
        res.status.is_success(),
        "status={:?}, stderr={}",
        res.status,
        String::from_utf8_lossy(&res.stderr_tail)
    );
    let out = String::from_utf8_lossy(&res.stdout.unwrap().bytes).into_owned();
    assert!(
        out.trim().ends_with("\t0") || out.trim().ends_with(" 0"),
        "Expected Cpus_allowed_list to be pinned to core 0, got: {out}"
    );
}

/// Item 8: `cleanup_stale` removes leftover `/dev/shm/.uj_rootfs_<dead_pid>_*` directories.
#[cfg(target_os = "linux")]
#[test]
fn test_linux_cleanup_stale_removes_dead_rootfs() {
    let sb = create_sandbox("auto", false).unwrap();
    let stale_dir = std::path::Path::new("/dev/shm").join(".uj_rootfs_99999999_deadbeef");
    let _ = std::fs::create_dir_all(&stale_dir);
    assert!(stale_dir.exists());

    let report = sb.cleanup_stale().unwrap();
    assert!(
        !stale_dir.exists(),
        "cleanup_stale should remove dead .uj_rootfs_<dead_pid>_* directory"
    );
    assert!(report.cleaned_directories >= 1);
}

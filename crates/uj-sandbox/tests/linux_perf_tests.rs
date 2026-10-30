//! Linux sandbox per-run overhead benchmark and accuracy regression tests (Round 14).
//!
//! The benchmark is `#[ignore]`d by default; run it explicitly with:
//!
//! ```bash
//! cargo test -p uj-sandbox --release --test linux_perf_tests -- --ignored --nocapture
//! ```
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::time::Instant;
use uj_core::run::{ExitStatus, KillReason, Limits, RunSpec, Stdio, Tier};
use uj_sandbox::{CancelToken, create_sandbox};

fn spec_for(argv: Vec<OsString>, work_dir: &std::path::Path, trusted: bool) -> RunSpec {
    let mut limits = Limits::new(2000, 256 * 1024 * 1024);
    // Shell pipelines in these tests need a few helper processes.
    limits.max_processes = 16;
    RunSpec {
        argv,
        env: vec![],
        work_dir: work_dir.to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits,
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted,
    }
}

struct Stats {
    mean_wall_ms: f64,
    p50_wall_ms: f64,
    mean_reported_cpu_us: f64,
    mean_reported_wall_us: f64,
}

fn bench(label: &str, spec: &RunSpec, iters: usize) -> Stats {
    let sb = create_sandbox("auto", false).unwrap();
    let cancel = CancelToken::new();
    // Warm-up
    for _ in 0..5 {
        let _ = sb.run(spec, &cancel).unwrap();
    }
    let mut walls = Vec::with_capacity(iters);
    let mut cpu_sum = 0u64;
    let mut rwall_sum = 0u64;
    for _ in 0..iters {
        let t = Instant::now();
        let res = sb.run(spec, &cancel).unwrap();
        walls.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(res.status.is_success(), "{label}: {:?}", res.status);
        cpu_sum += res.cpu_time_us;
        rwall_sum += res.wall_time_us;
    }
    let mut sorted = walls.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let stats = Stats {
        mean_wall_ms: walls.iter().sum::<f64>() / iters as f64,
        p50_wall_ms: sorted[iters / 2],
        mean_reported_cpu_us: cpu_sum as f64 / iters as f64,
        mean_reported_wall_us: rwall_sum as f64 / iters as f64,
    };
    eprintln!(
        "[bench] {label:<28} tier={:?} iters={iters} spawn->exit mean={:.3}ms p50={:.3}ms | reported cpu_time mean={:.0}us wall_time mean={:.0}us",
        sb.tier(),
        stats.mean_wall_ms,
        stats.p50_wall_ms,
        stats.mean_reported_cpu_us,
        stats.mean_reported_wall_us
    );
    stats
}

/// Per-run overhead benchmark: runs a trivial program 200 times through the sandbox.
#[test]
#[ignore]
fn bench_linux_sandbox_per_run_overhead() {
    let dir = tempfile::tempdir().unwrap();
    let iters: usize = std::env::var("UJ_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    // Inflate the supervisor's RSS a bit so fork() page-table copy costs are visible,
    // as in a real judge process holding test data / answer caches.
    let ballast_mb: usize = std::env::var("UJ_BENCH_BALLAST_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let mut ballast = vec![0u8; ballast_mb * 1024 * 1024];
    for i in (0..ballast.len()).step_by(4096) {
        ballast[i] = 1;
    }

    let prog = dir.path().join("prog_true");
    std::fs::copy("/usr/bin/true", &prog).unwrap();

    bench(
        "untrusted /bin/true",
        &spec_for(vec!["/bin/true".into()], dir.path(), false),
        iters,
    );
    bench(
        "untrusted box binary",
        &spec_for(vec![prog.clone().into_os_string()], dir.path(), false),
        iters,
    );
    bench(
        "trusted /bin/true",
        &spec_for(vec!["/bin/true".into()], dir.path(), true),
        iters,
    );
    std::hint::black_box(&ballast);
}

/// Reported CPU time of an empty program must not include sandbox setup/teardown
/// (namespaces, mounts, forks). With PID1-side accounting it should be well below 5 ms.
#[test]
fn test_linux_empty_program_cpu_time_excludes_setup() {
    let sb = create_sandbox("auto", false).unwrap();
    if sb.tier() != Tier::Full {
        eprintln!("skip: requires Full tier (cgroup v2)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let spec = spec_for(vec!["/bin/true".into()], dir.path(), false);
    let _ = sb.run(&spec, &cancel).unwrap();
    let mut min_cpu = u64::MAX;
    for _ in 0..10 {
        let res = sb.run(&spec, &cancel).unwrap();
        assert!(res.status.is_success());
        min_cpu = min_cpu.min(res.cpu_time_us);
    }
    assert!(
        min_cpu < 5_000,
        "empty program reported {min_cpu}us of CPU time; setup overhead leaked into accounting"
    );
}

/// memory.peak must reflect the current run, not earlier runs reusing the same cgroup.
#[test]
fn test_linux_memory_peak_is_per_run() {
    let sb = create_sandbox("auto", false).unwrap();
    if sb.tier() != Tier::Full {
        eprintln!("skip: requires Full tier (cgroup v2)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    // Touch ~96 MiB via a shell+dd pipeline buffered in memory.
    let big = spec_for(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "head -c 100000000 /dev/zero | tail -c 100000000 > /dev/null".into(),
        ],
        dir.path(),
        false,
    );
    let small = spec_for(vec!["/bin/true".into()], dir.path(), false);
    let big_res = sb.run(&big, &cancel).unwrap();
    assert!(big_res.status.is_success(), "{:?}", big_res.status);
    for _ in 0..3 {
        let small_res = sb.run(&small, &cancel).unwrap();
        assert!(small_res.status.is_success());
        assert!(
            small_res.memory_peak_bytes < big_res.memory_peak_bytes / 2
                || big_res.memory_peak_bytes < 32 * 1024 * 1024,
            "small run peak {} should not inherit big run peak {}",
            small_res.memory_peak_bytes,
            big_res.memory_peak_bytes
        );
    }
}

/// Submounts under read-only binds (e.g. /usr) must also be read-only, and the box stays writable.
#[test]
fn test_linux_rootfs_readonly_and_box_writable() {
    let sb = create_sandbox("auto", false).unwrap();
    if sb.tier() == Tier::None {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let spec = spec_for(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "touch /usr/uj_probe 2>/dev/null && echo USR_WRITABLE; \
             touch /uj_probe 2>/dev/null && echo ROOT_WRITABLE; \
             touch /etc/uj_probe 2>/dev/null && echo ETC_WRITABLE; \
             echo ok > /box/w.txt && echo BOX_OK; \
             echo ok > /tmp/w.txt && echo TMP_OK; \
             awk '$5 ~ \"^/usr\" && $6 !~ /(^|,)ro(,|$)/ {print \"RW_SUBMOUNT \" $5}' /proc/self/mountinfo"
                .into(),
        ],
        dir.path(),
        false,
    );
    let res = sb.run(&spec, &cancel).unwrap();
    let out = String::from_utf8_lossy(&res.stdout.unwrap().bytes).into_owned();
    assert!(!out.contains("USR_WRITABLE"), "{out}");
    assert!(!out.contains("ROOT_WRITABLE"), "{out}");
    assert!(!out.contains("ETC_WRITABLE"), "{out}");
    assert!(out.contains("BOX_OK"), "{out}");
    assert!(out.contains("TMP_OK"), "{out}");
    assert!(!out.contains("RW_SUBMOUNT"), "{out}");
}

/// Crashes inside the sandbox must still be reported as real signals (gotcha 3.1),
/// and CPU-bound loops must be stopped by the CPU limit.
#[test]
fn test_linux_signal_and_tle_semantics_preserved() {
    let sb = create_sandbox("auto", false).unwrap();
    if sb.tier() == Tier::None {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let seg = spec_for(
        vec!["/bin/sh".into(), "-c".into(), "kill -SEGV $$".into()],
        dir.path(),
        false,
    );
    let res = sb.run(&seg, &cancel).unwrap();
    assert_eq!(res.status, ExitStatus::Signaled(libc::SIGSEGV));

    let mut tle = spec_for(
        vec!["/bin/sh".into(), "-c".into(), "while :; do :; done".into()],
        dir.path(),
        false,
    );
    tle.limits = Limits::new(300, 128 * 1024 * 1024);
    tle.limits.wall_time_ms = 5000;
    let t = Instant::now();
    let res = sb.run(&tle, &cancel).unwrap();
    assert_eq!(res.killed_by, Some(KillReason::CpuLimit), "{res:?}");
    assert!(res.cpu_time_us >= 300_000, "{res:?}");
    assert!(t.elapsed().as_millis() < 3000);
}

/// Output-limit is detected and large outputs are captured up to the limit.
#[test]
fn test_linux_ole_detected_quickly() {
    let sb = create_sandbox("auto", false).unwrap();
    if sb.tier() == Tier::None {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    let mut spec = spec_for(
        vec!["/bin/sh".into(), "-c".into(), "yes".into()],
        dir.path(),
        false,
    );
    spec.limits.output_bytes = 1024 * 1024;
    spec.limits.wall_time_ms = 10_000;
    spec.limits.cpu_time_ms = 10_000;
    let t = Instant::now();
    let res = sb.run(&spec, &cancel).unwrap();
    assert_eq!(res.killed_by, Some(KillReason::OutputLimit), "{res:?}");
    let out = res.stdout.unwrap();
    assert!(out.truncated);
    assert_eq!(out.bytes.len(), 1024 * 1024);
    assert!(t.elapsed().as_millis() < 5000);
}

/// Normal (non-limit) stdout and stderr capture are preserved.
#[test]
fn test_linux_stdout_stderr_capture() {
    let sb = create_sandbox("auto", false).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancelToken::new();
    for trusted in [false, true] {
        let spec = spec_for(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "head -c 300000 /dev/zero | tr '\\0' a; echo ERR >&2; exit 3".into(),
            ],
            dir.path(),
            trusted,
        );
        let res = sb.run(&spec, &cancel).unwrap();
        assert_eq!(res.status, ExitStatus::Exited(3));
        let out = res.stdout.unwrap();
        assert_eq!(out.bytes.len(), 300_000);
        assert!(!out.truncated);
        assert_eq!(String::from_utf8_lossy(&res.stderr_tail).trim(), "ERR");
        assert!(res.killed_by.is_none());
    }
}

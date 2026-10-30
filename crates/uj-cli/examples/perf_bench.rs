use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn bench_subtask_pattern_matching() {
    let mut names = Vec::with_capacity(10000);
    for i in 1..=2500 {
        names.push(format!("{i}"));
    }
    for i in 1..=2500 {
        names.push(format!("case_{i}"));
    }
    for i in 1..=2500 {
        names.push(format!("subtask1_{i}"));
    }
    for i in 1..=2500 {
        names.push(format!("foo_{i}"));
    }

    let patterns = vec![
        "1-1000".to_string(),
        "re:^case_\\d+$".to_string(),
        "subtask*".to_string(),
        "foo_100".to_string(),
    ];

    // Warmup
    let mut matches = 0;
    for name in &names {
        if uj_core::tests::test_matches_patterns(name, &patterns) {
            matches += 1;
        }
    }

    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut cnt = 0;
        for name in &names {
            if uj_core::tests::test_matches_patterns(name, &patterns) {
                cnt += 1;
            }
        }
        times.push(start.elapsed().as_micros());
        assert_eq!(cnt, matches);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "[Subtask Matching 10k items (Uncached/Per-test)] Median: {} µs ({} ms)",
        median,
        median as f64 / 1000.0,
    );

    // Precompiled PatternSet
    let set = uj_core::tests::PatternSet::compile(&patterns);
    let mut times_precompiled = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut cnt = 0;
        for name in &names {
            if set.is_match(name) {
                cnt += 1;
            }
        }
        times_precompiled.push(start.elapsed().as_micros());
        assert_eq!(cnt, matches);
    }
    times_precompiled.sort();
    let median_precompiled = times_precompiled[times_precompiled.len() / 2];
    println!(
        "[Subtask Matching 10k items (Precompiled PatternSet)] Median: {} µs ({} ms)",
        median_precompiled,
        median_precompiled as f64 / 1000.0,
    );
}

fn bench_checker_fcmp() {
    let mut ans_data = Vec::with_capacity(5 * 1024 * 1024);
    let mut out_ws_data = Vec::with_capacity(6 * 1024 * 1024);
    for i in 0..100_000 {
        ans_data.extend_from_slice(
            format!("line {i} with some tokens and numbers {}\n", i * 2).as_bytes(),
        );
        out_ws_data.extend_from_slice(
            format!("line {i} with some tokens and numbers {}   \r\n", i * 2).as_bytes(),
        );
    }
    // Distinct heap allocation with identical bytes (avoids std::ptr::eq slice short-circuit)
    let out_same_data = std::hint::black_box(ans_data.clone());
    assert_ne!(out_same_data.as_ptr(), ans_data.as_ptr());

    // 1. Distinct byte-identical buffers (fast path memcmp)
    let _ = uj_checker::builtin::check_fcmp(&out_same_data, &ans_data);
    let mut times_same = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_fcmp(
            std::hint::black_box(&out_same_data),
            std::hint::black_box(&ans_data),
        );
        times_same.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times_same.sort();
    println!(
        "[Checker fcmp 100k lines (distinct identical)] Median: {} µs ({} ms), Raw: {:?} µs",
        times_same[2],
        times_same[2] as f64 / 1000.0,
        times_same
    );

    // 2. Trailing-whitespace-differing buffers (full SimdLines scan)
    let _ = uj_checker::builtin::check_fcmp(&out_ws_data, &ans_data);
    let mut times_ws = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_fcmp(
            std::hint::black_box(&out_ws_data),
            std::hint::black_box(&ans_data),
        );
        times_ws.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times_ws.sort();
    println!(
        "[Checker fcmp 100k lines (trailing-ws SimdLines)] Median: {} µs ({} ms), Raw: {:?} µs",
        times_ws[2],
        times_ws[2] as f64 / 1000.0,
        times_ws
    );
}

fn bench_checker_hcmp() {
    let mut out_data = Vec::with_capacity(4 * 1024 * 1024);
    let mut ans_data = Vec::with_capacity(4 * 1024 * 1024);
    for i in 0..100_000 {
        out_data.extend_from_slice(format!("+000{}12345678901234567890 ", i).as_bytes());
        ans_data.extend_from_slice(format!("{}12345678901234567890\n", i).as_bytes());
    }

    // Warmup
    let _ = uj_checker::builtin::check_hcmp(&out_data, &ans_data);

    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_hcmp(
            std::hint::black_box(&out_data),
            std::hint::black_box(&ans_data),
        );
        times.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "[Checker hcmp 100k BigInts (distinct)] Median: {} µs ({} ms), Raw: {:?} µs",
        median,
        median as f64 / 1000.0,
        times
    );
}

fn bench_checker_ncmp() {
    let mut out_data = Vec::with_capacity(2 * 1024 * 1024);
    let mut ans_data = Vec::with_capacity(2 * 1024 * 1024);
    for i in 0..100_000 {
        out_data.extend_from_slice(format!("-{} ", i * 17).as_bytes());
        ans_data.extend_from_slice(format!("-{}\n", i * 17).as_bytes());
    }

    // Warmup
    let _ = uj_checker::builtin::check_ncmp(&out_data, &ans_data);

    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_ncmp(
            std::hint::black_box(&out_data),
            std::hint::black_box(&ans_data),
        );
        times.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "[Checker ncmp 100k Ints (distinct)] Median: {} µs ({} ms), Raw: {:?} µs",
        median,
        median as f64 / 1000.0,
        times
    );
}

fn bench_checker_wcmp() {
    let mut ans_data = Vec::with_capacity(2 * 1024 * 1024);
    let mut out_ws_data = Vec::with_capacity(6 * 1024 * 1024);
    for i in 0..100_000 {
        ans_data.extend_from_slice(format!("word_{i}\n").as_bytes());
        out_ws_data.extend_from_slice(format!("        word_{i} \r\n").as_bytes());
    }
    let out_same_data = std::hint::black_box(ans_data.clone());
    assert_ne!(out_same_data.as_ptr(), ans_data.as_ptr());

    // 1. Distinct byte-identical buffers (fast path memcmp)
    let _ = uj_checker::builtin::check_wcmp(&out_same_data, &ans_data);
    let mut times_same = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_wcmp(
            std::hint::black_box(&out_same_data),
            std::hint::black_box(&ans_data),
        );
        times_same.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times_same.sort();
    println!(
        "[Checker wcmp 100k Words (distinct identical)] Median: {} µs ({} ms), Raw: {:?} µs",
        times_same[2],
        times_same[2] as f64 / 1000.0,
        times_same
    );

    // 2. Whitespace-differing buffers (full SWAR tokenizer)
    let _ = uj_checker::builtin::check_wcmp(&out_ws_data, &ans_data);
    let mut times_ws = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_wcmp(
            std::hint::black_box(&out_ws_data),
            std::hint::black_box(&ans_data),
        );
        times_ws.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times_ws.sort();
    println!(
        "[Checker wcmp 100k Words (ws-diff tokenizer)] Median: {} µs ({} ms), Raw: {:?} µs",
        times_ws[2],
        times_ws[2] as f64 / 1000.0,
        times_ws
    );
}

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn setup_aplusb_benchmark_problem(dir: &Path, count: usize) {
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::create_dir_all(dir.join("solutions")).unwrap();

    let problem_toml = r#"
schema = 1
name = "perf_aplusb"
type = "traditional"

[limits]
time = "1s"
memory = "512MiB"

[io]
mode = "stdio"

[checker]
type = "builtin"
name = "wcmp"
"#;
    fs::write(dir.join("problem.toml"), problem_toml).unwrap();

    let sol = r#"
#include <iostream>
int main() {
    long long a, b;
    if (std::cin >> a >> b) {
        std::cout << a + b << "\n";
    }
    return 0;
}
"#;
    fs::write(dir.join("solutions/ac.cpp"), sol).unwrap();

    for i in 1..=count {
        fs::write(
            dir.join(format!("tests/{i}.in")),
            format!("{i} {}\n", i * 2),
        )
        .unwrap();
        fs::write(dir.join(format!("tests/{i}.ans")), format!("{}\n", i * 3)).unwrap();
    }
}

fn resolve_uj_binary(root: &Path) -> Option<PathBuf> {
    if let Some(arg) = std::env::args().nth(1) {
        let p = PathBuf::from(arg);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(var) = std::env::var("UJ_BIN") {
        let p = PathBuf::from(var);
        if p.exists() {
            return Some(p);
        }
    }
    let exe_name = if cfg!(windows) { "uj.exe" } else { "uj" };
    if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
        let candidate = Path::new(&target_dir).join("release").join(exe_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let default_candidate = root.join("target/release").join(exe_name);
    if default_candidate.exists() {
        return Some(default_candidate);
    }
    None
}

fn verify_judge_report_json(output: &[u8], expected_count: usize) {
    let json: serde_json::Value =
        serde_json::from_slice(output).expect("uj judge output is not valid json");
    let v_str = json.get("verdict").and_then(|v| v.as_str());
    assert!(
        v_str == Some("AC") || v_str == Some("Accepted"),
        "Submission verdict must be Accepted/AC (got {:?})",
        v_str
    );
    let tests = json
        .get("tests")
        .and_then(|v| v.as_array())
        .expect("Missing tests array in report");
    assert_eq!(
        tests.len(),
        expected_count,
        "Number of tested items must match expected count"
    );
    for t in tests {
        let t_v = t.get("verdict").and_then(|v| v.as_str());
        assert!(
            t_v == Some("AC") || t_v == Some("Accepted"),
            "Individual test must be Accepted/AC (got {:?})",
            t_v
        );
        let tier = t.get("sandbox_tier").and_then(|v| v.as_str());
        assert!(
            tier.is_some() && tier != Some("none") && tier != Some("None"),
            "sandbox_tier must be valid active tier (got {:?})",
            tier
        );
    }
}

fn bench_e2e_judging(count: usize) {
    let root = get_workspace_root();
    let perf_dir = root.join(".uj/perf").join(format!("aplusb_{count}"));
    setup_aplusb_benchmark_problem(&perf_dir, count);

    let uj_bin = match resolve_uj_binary(&root) {
        Some(b) => b,
        None => {
            eprintln!("Warning: release binary 'uj' not found, skipping e2e benchmark");
            return;
        }
    };

    let sol_path = perf_dir.join("solutions/ac.cpp");

    // 1. Warmup / initial compile
    let warmup_out = Command::new(&uj_bin)
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg(&perf_dir)
        .arg("--format")
        .arg("json")
        .arg("--precise")
        .arg("false")
        .arg("-j")
        .arg("1")
        .output()
        .expect("failed to run uj judge warmup");
    assert!(warmup_out.status.success());
    verify_judge_report_json(&warmup_out.stdout, count);

    // 2. Measure -j 1 (3 runs)
    let mut times_j1 = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let out = Command::new(&uj_bin)
            .arg("judge")
            .arg(&sol_path)
            .arg("-p")
            .arg(&perf_dir)
            .arg("--format")
            .arg("json")
            .arg("--precise")
            .arg("false")
            .arg("-j")
            .arg("1")
            .output()
            .expect("failed to run uj judge");
        let elapsed = start.elapsed().as_millis();
        times_j1.push(elapsed);
        assert!(out.status.success());
        verify_judge_report_json(&out.stdout, count);
    }
    times_j1.sort();

    // 3. Measure -j 4 (3 runs)
    let mut times_j4 = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let out = Command::new(&uj_bin)
            .arg("judge")
            .arg(&sol_path)
            .arg("-p")
            .arg(&perf_dir)
            .arg("--format")
            .arg("json")
            .arg("--precise")
            .arg("false")
            .arg("-j")
            .arg("4")
            .output()
            .expect("failed to run uj judge");
        let elapsed = start.elapsed().as_millis();
        times_j4.push(elapsed);
        assert!(out.status.success());
        verify_judge_report_json(&out.stdout, count);
    }
    times_j4.sort();

    println!(
        "[E2E Judge {count} tests -j 1] Median: {} ms, Raw: {:?} ms (Verified {} tests Accepted & Active Tier)",
        times_j1[1], times_j1, count
    );
    println!(
        "[E2E Judge {count} tests -j 4] Median: {} ms, Raw: {:?} ms (Verified {} tests Accepted & Active Tier)",
        times_j4[1], times_j4, count
    );
}

fn bench_tui_multi_events() {
    let sub_count = 100usize;
    let tests_per_sub = 200usize;
    let test_names: Vec<String> = (1..=tests_per_sub).map(|i| format!("case_{i}")).collect();
    let metas: Vec<uj_tui::multi::SubmissionMeta> = (0..sub_count)
        .map(|id| uj_tui::multi::SubmissionMeta {
            id,
            problem: "aplusb".to_string(),
            name: format!("player_{id}"),
            test_names: test_names.clone(),
        })
        .collect();

    let mut events = Vec::with_capacity(sub_count * tests_per_sub * 2);
    for id in 0..sub_count {
        let sub_ref = uj_report::result::SubmissionRef {
            id,
            problem: "aplusb".to_string(),
            name: format!("player_{id}"),
        };
        for t in 1..=tests_per_sub {
            let tid = format!("case_{t}");
            events.push(uj_report::result::PoolEvent {
                submission: sub_ref.clone(),
                inner: uj_report::result::JudgeEvent::TestStart {
                    id: tid.clone(),
                    subtask: Some(1),
                    slot: Some((id + t) % 8),
                },
            });
            events.push(uj_report::result::PoolEvent {
                submission: sub_ref.clone(),
                inner: uj_report::result::JudgeEvent::TestDone(
                    uj_report::result::SingleTestResult {
                        id: tid,
                        subtask: Some(1),
                        verdict: uj_core::verdict::Verdict::Accepted,
                        score: 1.0,
                        time_ms: 12,
                        wall_ms: 15,
                        memory_kb: 2048,
                        exit: Default::default(),
                        message: None,
                        checker: None,
                        diff: None,
                        timing: Default::default(),
                        sandbox_tier: Some(uj_core::run::Tier::Full),
                    },
                ),
            });
        }
    }

    let mut times = Vec::new();
    for _ in 0..5 {
        let mut app = uj_tui::multi::MultiApp::new(
            "Bench Contest",
            &metas,
            8,
            vec![None; 8],
            uj_sandbox::CancelToken::new(),
        );
        let start = Instant::now();
        for ev in events.iter().cloned() {
            app.handle_event(ev);
        }
        times.push(start.elapsed().as_micros());
        assert_eq!(app.completed_tests, sub_count * tests_per_sub);
    }
    times.sort();
    println!(
        "[TUI MultiApp 40k PoolEvents (100x200)] Median: {} µs ({:.2} ms), Raw: {:?} µs",
        times[2],
        times[2] as f64 / 1000.0,
        times
    );
}

fn bench_author_pipeline_and_stress() {
    let root = get_workspace_root();
    let Some(uj_bin) = resolve_uj_binary(&root) else {
        return;
    };
    let bench_dir = root.join(".uj/perf/author_bench");
    let _ = fs::remove_dir_all(&bench_dir);

    let init_out = Command::new(&uj_bin)
        .arg("init")
        .arg(&bench_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to run uj init");
    assert!(init_out.status.success());

    // Initial cold pipeline build + gen + validate + ans + verify
    let start_cold = Instant::now();
    let pipe_out = Command::new(&uj_bin)
        .arg("pipeline")
        .arg("-p")
        .arg(&bench_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to run uj pipeline");
    let cold_ms = start_cold.elapsed().as_millis();
    assert!(
        pipe_out.status.success(),
        "uj pipeline failed: {}",
        String::from_utf8_lossy(&pipe_out.stderr)
    );

    // Warm validate cache-hit latency
    let start_val = Instant::now();
    let val_out = Command::new(&uj_bin)
        .arg("validate")
        .arg("-p")
        .arg(&bench_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to run uj validate");
    let warm_val_ms = start_val.elapsed().as_millis();
    assert!(val_out.status.success());

    // Warm ans cache-hit latency
    let start_ans = Instant::now();
    let ans_out = Command::new(&uj_bin)
        .arg("ans")
        .arg("-p")
        .arg(&bench_dir)
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to run uj ans");
    let warm_ans_ms = start_ans.elapsed().as_millis();
    assert!(ans_out.status.success());

    // Concurrent stress test (20 rounds std vs brute)
    let start_stress = Instant::now();
    let stress_out = Command::new(&uj_bin)
        .arg("stress")
        .arg("-p")
        .arg(&bench_dir)
        .arg("--gen")
        .arg("gen.cpp")
        .arg("--sol")
        .arg("solutions/std.cpp")
        .arg("--brute")
        .arg("solutions/brute.cpp")
        .arg("-n")
        .arg("20")
        .arg("--format")
        .arg("json")
        .output()
        .expect("failed to run uj stress");
    let stress_ms = start_stress.elapsed().as_millis();
    assert!(stress_out.status.success());

    println!(
        "[Authoring] Cold pipeline: {cold_ms} ms | Warm validate: {warm_val_ms} ms | Warm ans: {warm_ans_ms} ms | Stress 20 rounds: {stress_ms} ms"
    );
}

fn main() {
    println!("=== UJ Performance Benchmark ===");
    bench_subtask_pattern_matching();
    bench_checker_fcmp();
    bench_checker_hcmp();
    bench_checker_ncmp();
    bench_checker_wcmp();
    bench_tui_multi_events();
    let count = std::env::var("UJ_BENCH_TESTS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    bench_e2e_judging(count);
    bench_author_pipeline_and_stress();
    let _ = fs::remove_dir_all(get_workspace_root().join(".uj/perf"));
    println!("=== Benchmark Complete ===");
}

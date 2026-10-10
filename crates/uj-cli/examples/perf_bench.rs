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
    let mut data = Vec::with_capacity(5 * 1024 * 1024);
    for i in 0..100_000 {
        data.extend_from_slice(
            format!("line {i} with some tokens and numbers {}\r\n", i * 2).as_bytes(),
        );
    }

    // Warmup
    let _ = uj_checker::builtin::check_fcmp(&data, &data);

    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_fcmp(&data, &data);
        times.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "[Checker fcmp 100k lines] Median: {} µs ({} ms), Raw: {:?} µs",
        median,
        median as f64 / 1000.0,
        times
    );
}

fn bench_checker_hcmp() {
    let mut data = Vec::with_capacity(4 * 1024 * 1024);
    for i in 0..100_000 {
        data.extend_from_slice(format!("+000{}12345678901234567890 ", i).as_bytes());
    }

    // Warmup
    let _ = uj_checker::builtin::check_hcmp(&data, &data);

    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let res = uj_checker::builtin::check_hcmp(&data, &data);
        times.push(start.elapsed().as_micros());
        assert_eq!(res.verdict, uj_core::verdict::Verdict::Accepted);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "[Checker hcmp 100k BigInts] Median: {} µs ({} ms), Raw: {:?} µs",
        median,
        median as f64 / 1000.0,
        times
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

fn main() {
    println!("=== UJ Performance Benchmark ===");
    bench_subtask_pattern_matching();
    bench_checker_fcmp();
    bench_checker_hcmp();
    let count = std::env::var("UJ_BENCH_TESTS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    bench_e2e_judging(count);
    println!("=== Benchmark Complete ===");
}

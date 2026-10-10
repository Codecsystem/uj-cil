//! Contest batch judging and leaderboard aggregation (Phase 8).
//!
//! Supports evaluating multiple contestants across multiple problems in parallel
//! with standard OI/LemonLime directory structures.

use crate::cli::{Cli, OutputFormat, SandboxMode};
use crate::error::{EXIT_INTERRUPTED, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use uj_core::config::ProblemConfig;
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_engine::scheduler::{JudgeOptions, Scheduler};
use uj_report::result::SubmissionJudgeReport;
use uj_sandbox::CancelToken;

/// A problem discovered in a contest.
struct ContestProblem {
    name: String,
    dir: PathBuf,
    config: ProblemConfig,
    tests: Vec<TestCase>,
}

/// A contestant's aggregated results across the contest.
#[derive(Debug, Clone)]
pub struct ContestantStanding {
    pub rank: usize,
    pub name: String,
    pub total_score: f64,
    pub total_time_ms: u64,
    pub problem_results: HashMap<String, (Verdict, f64, u64)>, // problem -> (verdict, score, time)
}

/// Discovers all problems in the contest directory.
fn discover_problems(contest_dir: &Path) -> Vec<ContestProblem> {
    let mut problems = Vec::new();
    let candidates = [
        contest_dir.join("problems"),
        contest_dir.join("data"),
        contest_dir.to_path_buf(),
    ];

    let mut seen_names = std::collections::HashSet::new();

    for candidate_dir in &candidates {
        if !candidate_dir.exists() {
            continue;
        }

        let toml_p = candidate_dir.join("problem.toml");
        if toml_p.exists()
            && let Ok(content) = fs::read_to_string(&toml_p)
            && let Ok(cfg) = ProblemConfig::from_toml_str(&content, Some(&toml_p))
            && !seen_names.contains(&cfg.name)
        {
            let tests = uj_core::tests::discover_tests(candidate_dir, &cfg).unwrap_or_default();
            seen_names.insert(cfg.name.clone());
            problems.push(ContestProblem {
                name: cfg.name.clone(),
                dir: candidate_dir.clone(),
                config: cfg,
                tests,
            });
        }

        if let Ok(entries) = fs::read_dir(candidate_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let sub_toml = p.join("problem.toml");
                if p.is_dir()
                    && sub_toml.exists()
                    && let Ok(content) = fs::read_to_string(&sub_toml)
                    && let Ok(cfg) = ProblemConfig::from_toml_str(&content, Some(&sub_toml))
                    && !seen_names.contains(&cfg.name)
                {
                    let tests = uj_core::tests::discover_tests(&p, &cfg).unwrap_or_default();
                    seen_names.insert(cfg.name.clone());
                    problems.push(ContestProblem {
                        name: cfg.name.clone(),
                        dir: p.clone(),
                        config: cfg,
                        tests,
                    });
                }
            }
        }
    }

    problems
}

/// Discovers participant submissions in standard or Lemon/LemonLime directories:
/// - `<contest>/source/<player>/<problem>/<problem>.cpp` (Lemon standard)
/// - `<contest>/source/<player>/<problem>.cpp`
/// - `<contest>/players/<player>/<problem>.cpp`
/// - `<contest>/submissions/<player>/<problem>.cpp`
fn discover_submissions(contest_dir: &Path) -> BTreeMap<String, HashMap<String, PathBuf>> {
    let mut submissions: BTreeMap<String, HashMap<String, PathBuf>> = BTreeMap::new();
    let candidates = ["source", "players", "submissions"];

    let valid_exts = ["cpp", "cc", "cxx", "c", "rs", "py"];

    for folder_name in &candidates {
        let root = contest_dir.join(folder_name);
        if !root.is_dir() {
            continue;
        }

        let Ok(player_dirs) = fs::read_dir(&root) else {
            continue;
        };

        for player_entry in player_dirs.flatten() {
            let player_path = player_entry.path();
            if !player_path.is_dir() {
                continue;
            }
            let player_name = player_entry.file_name().to_string_lossy().to_string();

            // Look inside player_path
            if let Ok(items) = fs::read_dir(&player_path) {
                for item in items.flatten() {
                    let item_path = item.path();
                    if item_path.is_dir() {
                        // Lemon structure: source/<player>/<problem>/<problem>.cpp
                        let prob_name = item.file_name().to_string_lossy().to_string();
                        if let Ok(prob_files) = fs::read_dir(&item_path) {
                            for f in prob_files.flatten() {
                                let f_path = f.path();
                                if let Some(ext) = f_path.extension().and_then(|e| e.to_str())
                                    && valid_exts.contains(&ext.to_lowercase().as_str())
                                {
                                    submissions
                                        .entry(player_name.clone())
                                        .or_default()
                                        .entry(prob_name.clone())
                                        .or_insert(f_path);
                                }
                            }
                        }
                    } else if item_path.is_file() {
                        // Flat player dir: source/<player>/<problem>.cpp
                        if let (Some(stem), Some(ext)) = (
                            item_path.file_stem().and_then(|s| s.to_str()),
                            item_path.extension().and_then(|e| e.to_str()),
                        ) && valid_exts.contains(&ext.to_lowercase().as_str())
                        {
                            submissions
                                .entry(player_name.clone())
                                .or_default()
                                .entry(stem.to_string())
                                .or_insert(item_path);
                        }
                    }
                }
            }
        }
    }

    submissions
}

pub fn run_contest(
    contest_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    if !contest_dir.exists() {
        eprintln!(
            "Contest directory '{}' does not exist.",
            contest_dir.display()
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let problems = discover_problems(contest_dir);
    if problems.is_empty() {
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "NoProblemsFound",
                    "message": format!("No problem.toml found in '{}'", contest_dir.display()),
                    "hint": "Check contest directory layout (e.g. problems/<p>/problem.toml)"
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!(
                "No problems (problem.toml) found in '{}'.",
                contest_dir.display()
            );
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let submissions = discover_submissions(contest_dir);
    if submissions.is_empty() {
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "NoSubmissionsFound",
                    "message": format!("No player submissions found in '{}/source' or '{}/players'", contest_dir.display(), contest_dir.display()),
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!(
                "No participant submissions found in '{}/source' or '{}/players'.",
                contest_dir.display(),
                contest_dir.display()
            );
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = match cli.sandbox {
        SandboxMode::Auto => "auto",
        SandboxMode::Full => "full",
        SandboxMode::Degraded => "degraded",
        SandboxMode::None => "none",
    };

    let judge_options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: cli.fail_fast,
        precise: cli.precise,
    };

    let compile_session = uj_engine::compiler::CompileSession::new();
    let scheduler = Scheduler::with_session(judge_options, compile_session.clone());
    let mut all_reports: Vec<SubmissionJudgeReport> = Vec::new();
    let mut standings_map: HashMap<String, ContestantStanding> = HashMap::new();

    // Initialize standings
    for player in submissions.keys() {
        standings_map.insert(
            player.clone(),
            ContestantStanding {
                rank: 0,
                name: player.clone(),
                total_score: 0.0,
                total_time_ms: 0,
                problem_results: HashMap::new(),
            },
        );
    }

    if format == OutputFormat::Human {
        println!(
            "Judging contest '{}' ({} problems, {} contestants)...",
            contest_dir.display(),
            problems.len(),
            submissions.len()
        );
    }

    // Evaluate all (player, problem) pairs
    for prob in &problems {
        for (player, player_subs) in &submissions {
            if cancel.is_cancelled() {
                return ExitCode::from(EXIT_INTERRUPTED);
            }

            if let Some(source_path) = player_subs.get(&prob.name) {
                if format == OutputFormat::Human {
                    println!("Evaluating {} / {} ...", player, prob.name);
                }

                // Compile
                let compile_res = compile_session.compile(source_path, cli.no_cache);
                let compiled = match compile_res {
                    Ok(c) => c,
                    Err(e) => {
                        let ce_report = SubmissionJudgeReport {
                            schema_version: 1,
                            problem: prob.name.clone(),
                            submission: format!("{player} - {}", prob.name),
                            verdict: Verdict::CompileError,
                            score: 0.0,
                            max_time_ms: 0,
                            max_memory_kb: 0,
                            subtasks: vec![],
                            tests: vec![],
                        };
                        all_reports.push(ce_report);
                        if let Some(st) = standings_map.get_mut(player) {
                            st.problem_results
                                .insert(prob.name.clone(), (Verdict::CompileError, 0.0, 0));
                        }
                        if format == OutputFormat::Human {
                            eprintln!("  -> Compile Error: {e}");
                        }
                        continue;
                    }
                };

                let sub_display_name = format!("{player} - {}", prob.name);
                let report_res = scheduler.judge_submission(
                    &prob.dir,
                    &prob.config,
                    &compiled,
                    &sub_display_name,
                    prob.tests.clone(),
                    cli.jobs,
                    None,
                    cancel,
                );

                match report_res {
                    Ok(report) => {
                        if let Some(st) = standings_map.get_mut(player) {
                            st.total_score += report.score;
                            st.total_time_ms += report.max_time_ms;
                            st.problem_results.insert(
                                prob.name.clone(),
                                (report.verdict, report.score, report.max_time_ms),
                            );
                        }
                        all_reports.push(report);
                    }
                    Err(e) => {
                        eprintln!("  -> Judging failed for {player} / {}: {e}", prob.name);
                    }
                }
            } else if let Some(st) = standings_map.get_mut(player) {
                // Not submitted
                st.problem_results
                    .insert(prob.name.clone(), (Verdict::Skipped, 0.0, 0));
            }
        }
    }

    // Rank standings: Score descending, total time ascending
    let mut standings: Vec<ContestantStanding> = standings_map.into_values().collect();
    standings.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.total_time_ms.cmp(&b.total_time_ms))
    });

    for (idx, st) in standings.iter_mut().enumerate() {
        st.rank = idx + 1;
    }

    // Export HTML and CSV reports
    let html_path = contest_dir.join("contest_report.html");
    if let Ok(html_content) = uj_report::generate_multi_html(&all_reports, true) {
        let _ = fs::write(&html_path, html_content);
    }

    let csv_path = contest_dir.join("contest_report.csv");
    let csv_content = uj_report::generate_multi_csv(&all_reports);
    let _ = fs::write(&csv_path, csv_content);

    // Output results
    if format == OutputFormat::Json {
        let standings_json: Vec<serde_json::Value> = standings
            .iter()
            .map(|st| {
                let prob_map: serde_json::Map<String, serde_json::Value> = st
                    .problem_results
                    .iter()
                    .map(|(p, (v, s, t))| {
                        (
                            p.clone(),
                            serde_json::json!({
                                "verdict": v,
                                "score": s,
                                "time_ms": t,
                            }),
                        )
                    })
                    .collect();

                serde_json::json!({
                    "rank": st.rank,
                    "name": st.name,
                    "total_score": st.total_score,
                    "total_time_ms": st.total_time_ms,
                    "problems": prob_map,
                })
            })
            .collect();

        let out = serde_json::json!({
            "ok": true,
            "contest_dir": contest_dir.to_string_lossy().to_string(),
            "problems_count": problems.len(),
            "contestants_count": standings.len(),
            "standings": standings_json,
            "html_report": html_path.to_string_lossy().to_string(),
            "csv_report": csv_path.to_string_lossy().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("\n=== Contest Leaderboard ===");
        print!(
            "{:<6} {:<16} {:<12} {:<10}",
            "Rank", "Contestant", "Total Score", "Time"
        );
        for p in &problems {
            print!(" {:<12}", p.name);
        }
        println!();
        println!("{}", "-".repeat(48 + problems.len() * 13));

        for st in &standings {
            print!(
                "{:<6} {:<16} {:<12.1} {:<10}",
                st.rank,
                st.name,
                st.total_score,
                format!("{} ms", st.total_time_ms)
            );
            for p in &problems {
                if let Some((v, sc, _)) = st.problem_results.get(&p.name) {
                    print!(" {:<12}", format!("{:.0} ({})", sc, v.short_code()));
                } else {
                    print!(" {:<12}", "-");
                }
            }
            println!();
        }
        println!();
        println!("Reports exported:");
        println!("  - HTML: {}", html_path.display());
        println!("  - CSV:  {}", csv_path.display());
    }

    ExitCode::from(EXIT_SUCCESS)
}

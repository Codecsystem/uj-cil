//! Contest batch judging and leaderboard aggregation (Phase 8).
//!
//! Supports evaluating multiple contestants across multiple problems in parallel
//! with standard OI/LemonLime directory structures.

use crate::cli::{Cli, OutputFormat};
use crate::error::{EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG, emit_error, emit_interrupted};
use crossbeam_channel::unbounded;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use uj_core::config::ProblemConfig;
use uj_core::tests::TestCase;
use uj_core::verdict::Verdict;
use uj_engine::context::{ProblemContext, SubmissionContext};
use uj_engine::pool::{JudgePool, PoolOptions, PoolSubmission};
use uj_engine::topology::Topology;
use uj_report::result::{
    JudgeEvent, PoolEvent, SingleTestResult, SubmissionJudgeReport, SubmissionRef,
};
use uj_sandbox::CancelToken;
use uj_tui::multi::{SubmissionMeta, run_multi_tui};

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

    problems.sort_by(|a, b| a.name.cmp(&b.name));
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
                                    && uj_engine::compiler::Language::is_supported_extension(ext)
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
                        ) && uj_engine::compiler::Language::is_supported_extension(ext)
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
    problem_filter: &[String],
    contestant_filter: &[String],
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let compile_session = uj_engine::compiler::CompileSession::new();
    run_contest_with_session(
        contest_dir,
        problem_filter,
        contestant_filter,
        cli,
        format,
        cancel,
        &compile_session,
    )
}

pub fn run_contest_with_session(
    contest_dir: &Path,
    problem_filter: &[String],
    contestant_filter: &[String],
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
    compile_session: &uj_engine::compiler::CompileSession,
) -> ExitCode {
    if !contest_dir.exists() {
        emit_error(
            format,
            "ContestDirNotFound",
            format!(
                "Contest directory '{}' does not exist.",
                contest_dir.display()
            ),
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let mut problems = discover_problems(contest_dir);
    if !problem_filter.is_empty() {
        problems.retain(|p| {
            let dir_name = p
                .dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            problem_filter
                .iter()
                .any(|f| f == &p.name || f == &dir_name)
        });
    }
    if problems.is_empty() {
        emit_error(
            format,
            "NoProblemsFound",
            format!("No problem.toml found in '{}'", contest_dir.display()),
            Some("Check contest directory layout (e.g. problems/<p>/problem.toml)"),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let mut submissions = discover_submissions(contest_dir);
    if !contestant_filter.is_empty() {
        submissions.retain(|player, _| contestant_filter.iter().any(|f| f == player));
    }
    if submissions.is_empty() {
        emit_error(
            format,
            "NoSubmissionsFound",
            format!(
                "No player submissions found in '{}/source' or '{}/players'",
                contest_dir.display(),
                contest_dir.display()
            ),
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = cli.sandbox_mode_str();

    let compile_session = compile_session.clone();
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

    // -------------------------------------------------------------------------
    // Phase A: Parallel Compilation & Submission Context Preparation
    // -------------------------------------------------------------------------
    let mut problem_contexts: Vec<Result<Arc<ProblemContext>, String>> =
        Vec::with_capacity(problems.len());
    for (idx, prob) in problems.iter().enumerate() {
        match ProblemContext::prepare(idx, &prob.dir, &prob.config, &compile_session) {
            Ok(ctx) => problem_contexts.push(Ok(Arc::new(ctx))),
            Err(e) => {
                eprintln!("Problem prepare failed for {}: {e}", prob.name);
                problem_contexts.push(Err(e.to_string()));
            }
        }
    }

    #[derive(Clone)]
    struct SubCandidate {
        sub_id: usize,
        prob_idx: usize,
        prob_name: String,
        player: String,
        source_path: PathBuf,
    }

    let mut candidates = Vec::new();
    let mut sub_id_gen = 0;

    for (prob_idx, prob) in problems.iter().enumerate() {
        for (player, player_subs) in &submissions {
            if let Some(source_path) = player_subs.get(&prob.name) {
                candidates.push(SubCandidate {
                    sub_id: sub_id_gen,
                    prob_idx,
                    prob_name: prob.name.clone(),
                    player: player.clone(),
                    source_path: source_path.clone(),
                });
                sub_id_gen += 1;
            } else if let Some(st) = standings_map.get_mut(player) {
                st.problem_results
                    .insert(prob.name.clone(), (Verdict::Skipped, 0.0, 0));
            }
        }
    }

    enum PreparedCandidate {
        Ready(usize, String, String, PoolSubmission),
        CompileError(usize, String, String, SubmissionJudgeReport),
        Failed(usize, String, String, String),
    }

    let trace_phases = std::env::var("UJ_TRACE_PHASES")
        .map(|v| v == "1")
        .unwrap_or(false);
    let pipeline_start = Instant::now();

    let topology = Topology::detect(0, cli.jobs);
    let slots = topology.slots.len().max(1);
    let slot_cpus: Vec<Option<usize>> = topology.slots.iter().map(|s| s.cpu_core).collect();
    // Prefer non-slot / spare logical cores (E-cores / SMT siblings) for concurrent compilation
    // while `JudgePool::run_streaming` executes ready submissions on P-core judge slots.
    let spare_cores = topology.logical_cores.saturating_sub(slots);
    let compile_threads = if spare_cores >= 1 {
        spare_cores.clamp(1, 8)
    } else {
        (topology.logical_cores / 2).clamp(1, 4)
    };

    let total_submissions_count = candidates.len();
    let completed_submissions_count = Arc::new(AtomicUsize::new(0));
    let estimated_total_tests: usize = candidates
        .iter()
        .map(|c| problems.get(c.prob_idx).map(|p| p.tests.len()).unwrap_or(0))
        .sum();

    let use_tui = cli.tui && format == OutputFormat::Human && std::io::stderr().is_terminal();

    let (pool_event_tx, pool_event_rx) = unbounded::<PoolEvent>();
    let pool_events_opt =
        if use_tui || format == OutputFormat::Ndjson || format == OutputFormat::Human {
            Some(pool_event_tx.clone())
        } else {
            None
        };

    let pb = if format == OutputFormat::Human && !use_tui {
        let p = indicatif::ProgressBar::new(estimated_total_tests as u64);
        p.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        p.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} tests ({eta})")
                .unwrap()
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    let listener_handle = if format == OutputFormat::Human && !use_tui {
        let pb = pb.clone();
        let completed_counter = Arc::clone(&completed_submissions_count);
        let rx = pool_event_rx.clone();
        Some(std::thread::spawn(move || {
            while let Ok(pe) = rx.recv() {
                match pe.inner {
                    JudgeEvent::TestDone(_) => {
                        if let Some(ref p) = pb {
                            p.inc(1);
                        }
                    }
                    JudgeEvent::SubmissionDone(ref report) => {
                        let done = completed_counter.fetch_add(1, Ordering::SeqCst) + 1;
                        let msg = format!(
                            "[{done}/{total_submissions_count}] {} -> {} {:.1}",
                            pe.submission.name, report.verdict, report.score
                        );
                        if let Some(ref p) = pb {
                            p.println(msg);
                        } else {
                            println!("{msg}");
                        }
                    }
                    _ => {}
                }
            }
        }))
    } else {
        None
    };

    let ndjson_handle = if format == OutputFormat::Ndjson {
        let rx = pool_event_rx.clone();
        Some(std::thread::spawn(move || {
            use std::io::Write;
            let mut out = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout());
            while let Ok(ev) = rx.recv() {
                if serde_json::to_writer(&mut out, &ev).is_ok() {
                    let _ = out.write_all(b"\n");
                }
                if rx.is_empty() {
                    let _ = out.flush();
                }
            }
            let _ = out.flush();
        }))
    } else {
        None
    };

    let tui_handle = if use_tui {
        let sub_metas: Vec<SubmissionMeta> = candidates
            .iter()
            .map(|cand| {
                let test_names = if cand.prob_idx < problems.len() {
                    problems[cand.prob_idx]
                        .tests
                        .iter()
                        .map(|t| t.name.clone())
                        .collect()
                } else {
                    vec![]
                };
                SubmissionMeta {
                    id: cand.sub_id,
                    problem: cand.prob_name.clone(),
                    name: cand.player.clone(),
                    test_names,
                }
            })
            .collect();
        let title = format!(
            "Contest: {}",
            contest_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let cancel_tui = cancel.clone();
        let rx = pool_event_rx.clone();
        Some(std::thread::spawn(move || {
            if let Err(e) = run_multi_tui(
                &title,
                &sub_metas,
                slots,
                slot_cpus,
                rx.clone(),
                cancel_tui,
                true,
            ) {
                eprintln!(
                    "Warning: Contest TUI initialization failed ({e}); falling back to non-TUI execution."
                );
                while rx.recv().is_ok() {}
            }
        }))
    } else {
        None
    };

    // Stream compilations directly into `JudgePool::run_streaming` so judge slots begin executing
    // tests as soon as the first submission finishes compiling instead of waiting on a full barrier.
    let (cand_tx, cand_rx) = unbounded::<SubCandidate>();
    let (prep_tx, prep_rx) = unbounded::<PreparedCandidate>();
    let (stream_tx, stream_rx) = unbounded::<PoolSubmission>();

    let mut compile_workers = Vec::with_capacity(compile_threads);
    for _ in 0..compile_threads {
        let rx = cand_rx.clone();
        let tx = prep_tx.clone();
        let session = compile_session.clone();
        let no_cache = cli.no_cache;
        let precise = cli.effective_precise();
        let fail_fast = cli.fail_fast;
        let problem_contexts = problem_contexts.clone();
        let problems_meta: Vec<(String, Vec<TestCase>)> = problems
            .iter()
            .map(|p| (p.name.clone(), p.tests.clone()))
            .collect();
        let cancel = cancel.clone();

        let handle = std::thread::spawn(move || {
            while let Ok(candidate) = rx.recv() {
                if cancel.is_cancelled() {
                    break;
                }

                let p_ctx = match &problem_contexts[candidate.prob_idx] {
                    Ok(ctx) => Arc::clone(ctx),
                    Err(e) => {
                        let _ = tx.send(PreparedCandidate::Failed(
                            candidate.sub_id,
                            candidate.player,
                            candidate.prob_name,
                            format!("Problem setup error: {e}"),
                        ));
                        continue;
                    }
                };

                let compile_res = session.compile_problem_solution(
                    &p_ctx.problem_dir,
                    &p_ctx.config,
                    &candidate.source_path,
                    no_cache,
                );
                match compile_res {
                    Err(e) => {
                        let err_msg = e.to_string();
                        let ce_report = SubmissionJudgeReport {
                            schema_version: 1,
                            problem: candidate.prob_name.clone(),
                            submission: format!("{} - {}", candidate.player, candidate.prob_name),
                            verdict: Verdict::CompileError,
                            score: 0.0,
                            max_time_ms: 0,
                            max_memory_kb: 0,
                            subtasks: vec![],
                            tests: vec![SingleTestResult {
                                id: "compile".to_string(),
                                subtask: None,
                                verdict: Verdict::CompileError,
                                time_ms: 0,
                                wall_ms: 0,
                                memory_kb: 0,
                                score: 0.0,
                                exit: uj_report::result::ExitInfo {
                                    code: Some(1),
                                    signal: None,
                                    nt_status: None,
                                },
                                message: Some(err_msg.clone()),
                                checker: Some(err_msg),
                                diff: None,
                                timing: uj_report::result::TimingInfo::default(),
                                sandbox_tier: None,
                            }],
                        };
                        let _ = tx.send(PreparedCandidate::CompileError(
                            candidate.sub_id,
                            candidate.player,
                            candidate.prob_name,
                            ce_report,
                        ));
                    }
                    Ok(compiled) => {
                        let sub_display_name =
                            format!("{} - {}", candidate.player, candidate.prob_name);
                        let tests = problems_meta[candidate.prob_idx].1.clone();
                        match SubmissionContext::prepare(
                            candidate.sub_id,
                            &sub_display_name,
                            p_ctx,
                            &compiled,
                            tests,
                            fail_fast,
                        ) {
                            Ok(sub_ctx) => {
                                let _ = tx.send(PreparedCandidate::Ready(
                                    candidate.sub_id,
                                    candidate.player,
                                    candidate.prob_name,
                                    PoolSubmission {
                                        ctx: Arc::new(sub_ctx),
                                        events: None,
                                        precise,
                                    },
                                ));
                            }
                            Err(e) => {
                                let _ = tx.send(PreparedCandidate::Failed(
                                    candidate.sub_id,
                                    candidate.player,
                                    candidate.prob_name,
                                    e.to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        });
        compile_workers.push(handle);
    }

    drop(prep_tx);
    for cand in candidates.iter().cloned() {
        let _ = cand_tx.send(cand);
    }
    drop(cand_tx);

    let router_handle = {
        let pb_router = pb.clone();
        let completed_router = Arc::clone(&completed_submissions_count);
        let router_events = pool_events_opt.clone();
        let prob_test_counts: Vec<usize> = problems.iter().map(|p| p.tests.len()).collect();
        let cand_prob_idxs: Vec<usize> = candidates.iter().map(|c| c.prob_idx).collect();
        let is_human_no_tui = format == OutputFormat::Human && !use_tui;
        let emit_non_ready_events = use_tui || format == OutputFormat::Ndjson;

        std::thread::spawn(move || {
            let mut early_reports: HashMap<usize, SubmissionJudgeReport> = HashMap::new();
            let mut current_len = estimated_total_tests as u64;
            while let Ok(item) = prep_rx.recv() {
                match item {
                    PreparedCandidate::Ready(_sub_id, _player, _prob_name, mut pool_sub) => {
                        pool_sub.events = None;
                        let _ = stream_tx.send(pool_sub);
                    }
                    PreparedCandidate::CompileError(sub_id, player, prob_name, ce_report) => {
                        let skipped = cand_prob_idxs
                            .get(sub_id)
                            .and_then(|&pi| prob_test_counts.get(pi))
                            .copied()
                            .unwrap_or(0) as u64;
                        if skipped > 0
                            && let Some(ref p) = pb_router
                        {
                            current_len = current_len.saturating_sub(skipped);
                            p.set_length(current_len);
                        }
                        if is_human_no_tui {
                            let done = completed_router.fetch_add(1, Ordering::SeqCst) + 1;
                            let msg = format!(
                                "[{done}/{total_submissions_count}] {player} / {prob_name} -> CE 0.0"
                            );
                            if let Some(ref p) = pb_router {
                                p.println(msg);
                            } else {
                                println!("{msg}");
                            }
                        }
                        if emit_non_ready_events && let Some(ref ptx) = router_events {
                            let err_output =
                                ce_report.tests.first().and_then(|t| t.message.clone());
                            let _ = ptx.send(PoolEvent {
                                submission: SubmissionRef {
                                    id: sub_id,
                                    problem: prob_name.clone(),
                                    name: player.clone(),
                                },
                                inner: JudgeEvent::CompileDone {
                                    target: format!("{player}/{prob_name}"),
                                    success: false,
                                    duration_ms: 0,
                                    error_output: err_output,
                                },
                            });
                            let _ = ptx.send(PoolEvent {
                                submission: SubmissionRef {
                                    id: sub_id,
                                    problem: prob_name.clone(),
                                    name: player.clone(),
                                },
                                inner: JudgeEvent::SubmissionDone(ce_report.clone()),
                            });
                        }
                        early_reports.insert(sub_id, ce_report);
                    }
                    PreparedCandidate::Failed(sub_id, player, prob_name, err) => {
                        let skipped = cand_prob_idxs
                            .get(sub_id)
                            .and_then(|&pi| prob_test_counts.get(pi))
                            .copied()
                            .unwrap_or(0) as u64;
                        if skipped > 0
                            && let Some(ref p) = pb_router
                        {
                            current_len = current_len.saturating_sub(skipped);
                            p.set_length(current_len);
                        }
                        if is_human_no_tui {
                            let done = completed_router.fetch_add(1, Ordering::SeqCst) + 1;
                            let msg = format!(
                                "[{done}/{total_submissions_count}] {player} / {prob_name} -> JudgeFailed ({err})"
                            );
                            if let Some(ref p) = pb_router {
                                p.println(msg);
                            } else {
                                println!("{msg}");
                            }
                        }
                        let fail_report = SubmissionJudgeReport {
                            schema_version: 1,
                            problem: prob_name.clone(),
                            submission: format!("{player} - {prob_name}"),
                            verdict: Verdict::JudgeFailed,
                            score: 0.0,
                            max_time_ms: 0,
                            max_memory_kb: 0,
                            subtasks: vec![],
                            tests: vec![],
                        };
                        if emit_non_ready_events && let Some(ref ptx) = router_events {
                            let _ = ptx.send(PoolEvent {
                                submission: SubmissionRef {
                                    id: sub_id,
                                    problem: prob_name.clone(),
                                    name: player.clone(),
                                },
                                inner: JudgeEvent::SubmissionDone(fail_report.clone()),
                            });
                        }
                        early_reports.insert(sub_id, fail_report);
                    }
                }
            }
            drop(stream_tx);
            early_reports
        })
    };

    let pool = JudgePool::new(PoolOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        slots,
        window: (slots / 2).max(2),
        memory_budget: Topology::memory_budget_bytes(),
        pool_events: pool_events_opt,
    });

    let streamed_reports = pool.run_streaming(stream_rx, cancel);
    for h in compile_workers {
        let _ = h.join();
    }
    let mut final_reports = router_handle.join().unwrap_or_default();
    drop(pool);
    drop(pool_event_tx);

    if let Some(h) = listener_handle {
        let _ = h.join();
    }
    if let Some(h) = tui_handle {
        let _ = h.join();
    }
    if let Some(h) = ndjson_handle {
        let _ = h.join();
    }
    if trace_phases {
        eprintln!(
            "[TRACE] Streaming Compile + Judge/Precise: {} ms",
            pipeline_start.elapsed().as_millis()
        );
    }

    if let Some(p) = pb {
        p.finish_and_clear();
    }

    if cancel.is_cancelled() {
        return emit_interrupted(format, "Contest judging");
    }

    for (sub_id, rep_res) in streamed_reports {
        match rep_res {
            Ok(report) => {
                final_reports.insert(sub_id, report);
            }
            Err(e) => {
                let (player, prob_name) = candidates
                    .get(sub_id)
                    .map(|c| (c.player.as_str(), c.prob_name.as_str()))
                    .unwrap_or(("unknown", "unknown"));
                eprintln!("  -> Judging failed for {player} / {prob_name}: {e}");
                let fail_report = SubmissionJudgeReport {
                    schema_version: 1,
                    problem: prob_name.to_string(),
                    submission: format!("{player} - {prob_name}"),
                    verdict: Verdict::JudgeFailed,
                    score: 0.0,
                    max_time_ms: 0,
                    max_memory_kb: 0,
                    subtasks: vec![],
                    tests: vec![],
                };
                final_reports.insert(sub_id, fail_report);
            }
        }
    }

    for (sub_id, candidate) in candidates.iter().enumerate() {
        if let Some(report) = final_reports.remove(&sub_id) {
            if let Some(st) = standings_map.get_mut(&candidate.player) {
                st.total_score += report.score;
                st.total_time_ms += report.max_time_ms;
                st.problem_results.insert(
                    candidate.prob_name.clone(),
                    (report.verdict, report.score, report.max_time_ms),
                );
            }
            all_reports.push(report);
        }
    }

    // Rank standings: Score descending, total time ascending, name ascending
    let mut standings: Vec<ContestantStanding> = standings_map.into_values().collect();
    standings.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.total_time_ms.cmp(&b.total_time_ms))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut current_rank = 1;
    for i in 0..standings.len() {
        if i > 0 {
            let prev = &standings[i - 1];
            let curr = &standings[i];
            let same_score = (curr.total_score - prev.total_score).abs() < 1e-6;
            let same_time = curr.total_time_ms == prev.total_time_ms;
            if !(same_score && same_time) {
                current_rank = i + 1;
            }
        }
        standings[i].rank = current_rank;
    }

    // Export HTML and CSV reports
    let html_path = contest_dir.join("contest_report.html");
    let standing_rows: Vec<uj_report::ContestStandingRow> = standings
        .iter()
        .map(|st| {
            let verdict = if st.problem_results.is_empty() {
                Verdict::Skipped
            } else {
                st.problem_results
                    .values()
                    .fold(Verdict::Accepted, |acc, (v, _, _)| acc.merge(*v))
            };
            uj_report::ContestStandingRow {
                rank: st.rank,
                participant: st.name.clone(),
                total_score: st.total_score,
                total_time_ms: st.total_time_ms,
                verdict,
            }
        })
        .collect();
    match uj_report::generate_contest_html(&all_reports, &standing_rows) {
        Ok(html_content) => {
            if let Err(e) = fs::write(&html_path, html_content) {
                eprintln!(
                    "Failed to write HTML report to {}: {e}",
                    html_path.display()
                );
                return ExitCode::from(crate::error::EXIT_JUDGE_FAILED);
            }
        }
        Err(e) => {
            eprintln!("Failed to generate HTML report: {e}");
            return ExitCode::from(crate::error::EXIT_JUDGE_FAILED);
        }
    }

    let csv_path = contest_dir.join("contest_report.csv");
    let csv_content = uj_report::generate_multi_csv(&all_reports);
    if let Err(e) = fs::write(&csv_path, csv_content) {
        eprintln!("Failed to write CSV report to {}: {e}", csv_path.display());
        return ExitCode::from(crate::error::EXIT_JUDGE_FAILED);
    }

    // Output results
    if format == OutputFormat::Ndjson {
        use std::io::Write;
        #[derive(serde::Serialize)]
        struct ContestFinishedEvent<'a> {
            schema_version: u32,
            event: &'static str,
            ok: bool,
            standings: Vec<StandingJson<'a>>,
        }
        let out = ContestFinishedEvent {
            schema_version: 1,
            event: "contest_finished",
            ok: true,
            standings: standings_to_typed(&standings),
        };
        let stdout = std::io::stdout();
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let _ = serde_json::to_writer(&mut writer, &out);
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
        return ExitCode::from(EXIT_SUCCESS);
    } else if format == OutputFormat::Json {
        use std::io::Write;
        #[derive(serde::Serialize)]
        struct ContestJsonOutput<'a> {
            schema_version: u32,
            ok: bool,
            contest_dir: String,
            problems_count: usize,
            contestants_count: usize,
            standings: Vec<StandingJson<'a>>,
            html_report: String,
            csv_report: String,
            reports: &'a [SubmissionJudgeReport],
        }
        let out = ContestJsonOutput {
            schema_version: 1,
            ok: true,
            contest_dir: contest_dir.to_string_lossy().to_string(),
            problems_count: problems.len(),
            contestants_count: standings.len(),
            standings: standings_to_typed(&standings),
            html_report: html_path.to_string_lossy().to_string(),
            csv_report: csv_path.to_string_lossy().to_string(),
            reports: &all_reports,
        };
        let stdout = std::io::stdout();
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, stdout.lock());
        let _ = serde_json::to_writer_pretty(&mut writer, &out);
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    } else if format == OutputFormat::Human {
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

#[derive(serde::Serialize)]
struct StandingProblemJson {
    verdict: Verdict,
    score: f64,
    time_ms: u64,
}

#[derive(serde::Serialize)]
struct StandingJson<'a> {
    rank: usize,
    name: &'a str,
    total_score: f64,
    total_time_ms: u64,
    problems: BTreeMap<&'a str, StandingProblemJson>,
}

fn standings_to_typed(standings: &[ContestantStanding]) -> Vec<StandingJson<'_>> {
    standings
        .iter()
        .map(|st| {
            let problems: BTreeMap<&str, StandingProblemJson> = st
                .problem_results
                .iter()
                .map(|(p, (v, s, t))| {
                    (
                        p.as_str(),
                        StandingProblemJson {
                            verdict: *v,
                            score: *s,
                            time_ms: *t,
                        },
                    )
                })
                .collect();

            StandingJson {
                rank: st.rank,
                name: &st.name,
                total_score: st.total_score,
                total_time_ms: st.total_time_ms,
                problems,
            }
        })
        .collect()
}

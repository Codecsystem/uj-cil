//! CLI handlers for judge, last, show, and clean commands.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;

use crossbeam_channel::unbounded;
use serde::Serialize;
use uj_core::verdict::Verdict;
use uj_engine::compiler::CompileError;
use uj_engine::context::{ProblemContext, SubmissionContext};
use uj_engine::judge::JudgeOptions;
use uj_engine::pool::{JudgePool, PoolOptions, PoolSubmission};
use uj_engine::scheduler::Scheduler;
use uj_engine::topology::Topology;
use uj_report::human::{format_human_report, format_test_details};
use uj_report::result::{
    JudgeEvent, PoolEvent, SingleTestResult, SubmissionJudgeReport, SubmissionRef,
};
use uj_sandbox::CancelToken;
use uj_tui::multi::{SubmissionMeta, run_multi_tui};

use crate::cli::{Cli, OutputFormat};
use crate::error::{
    EXIT_COMPILE_ERROR, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG,
    emit_error, emit_interrupted, load_problem, validate_run_id,
};

static RUN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Serialize)]
#[serde(untagged)]
enum JudgeJsonItem {
    Report(SubmissionJudgeReport),
    CompileError {
        schema_version: u32,
        problem: String,
        submission: String,
        verdict: &'static str,
        score: f64,
        max_time_ms: u64,
        max_memory_kb: u64,
        subtasks: Vec<()>,
        tests: Vec<SingleTestResult>,
        compile_error: String,
    },
    JudgeFailed {
        schema_version: u32,
        problem: String,
        submission: String,
        verdict: &'static str,
        score: f64,
        max_time_ms: u64,
        max_memory_kb: u64,
        subtasks: Vec<()>,
        tests: Vec<()>,
        judge_failed_error: String,
    },
}

fn build_ce_struct(
    problem_name: &str,
    submission_name: &str,
    err_msg: &str,
) -> SubmissionJudgeReport {
    let ce_test = SingleTestResult {
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
        message: Some(err_msg.to_string()),
        checker: Some(err_msg.to_string()),
        diff: None,
        timing: uj_report::result::TimingInfo::default(),
        sandbox_tier: None,
    };
    SubmissionJudgeReport {
        schema_version: 1,
        problem: problem_name.to_string(),
        submission: submission_name.to_string(),
        verdict: Verdict::CompileError,
        score: 0.0,
        max_time_ms: 0,
        max_memory_kb: 0,
        subtasks: Vec::new(),
        tests: vec![ce_test],
    }
}

/// Save a report atomically to .uj/runs and update .uj/runs/last.json
pub(crate) fn save_run_report(
    problem_dir: &Path,
    report: &SubmissionJudgeReport,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let run_filename = format!("run-{ts}-{pid}-{seq}.json");

    let primary_runs_dir = problem_dir.join(".uj").join("runs");
    let mut targets = vec![primary_runs_dir];
    let same_dir = match (
        fs::canonicalize(problem_dir),
        std::env::current_dir().and_then(fs::canonicalize),
    ) {
        (Ok(p1), Ok(p2)) => p1 == p2,
        _ => problem_dir == Path::new(".") || problem_dir.as_os_str().is_empty(),
    };
    if !same_dir {
        targets.push(PathBuf::from(".uj").join("runs"));
    }

    for runs_dir in targets {
        fs::create_dir_all(&runs_dir)?;

        let tmp_file = runs_dir.join(format!(".tmp-run-{ts}-{pid}-{seq}.json"));
        fs::write(&tmp_file, &json)?;

        let final_path = runs_dir.join(&run_filename);
        let _ = fs::copy(&tmp_file, &final_path);
        let _ = fs::rename(&tmp_file, runs_dir.join("last.json"));
    }

    Ok(())
}

fn spawn_buffered_ndjson_writer() -> (
    crossbeam_channel::Sender<JudgeEvent>,
    thread::JoinHandle<()>,
) {
    let (tx, rx) = unbounded::<JudgeEvent>();
    let handle = thread::spawn(move || {
        let mut out = BufWriter::with_capacity(64 * 1024, std::io::stdout());
        while let Ok(ev) = rx.recv() {
            if serde_json::to_writer(&mut out, &ev).is_ok() {
                let _ = out.write_all(b"\n");
            }
            if rx.is_empty() {
                let _ = out.flush();
            }
        }
        let _ = out.flush();
    });
    (tx, handle)
}

pub fn run_judge(
    sources: &[PathBuf],
    problem_dir: &Path,
    cli: &Cli,
    format: OutputFormat,
    cancel: &CancelToken,
) -> ExitCode {
    let (_config_path, _config_content, problem_config) = match load_problem(problem_dir, format) {
        Ok(res) => res,
        Err(code) => return code,
    };

    let tests = match uj_core::tests::discover_tests(problem_dir, &problem_config) {
        Ok(t) => t,
        Err(e) => {
            emit_error(
                format,
                "IoError",
                format!("Failed to discover tests: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    if tests.is_empty() {
        emit_error(
            format,
            "NoTestsFound",
            format!(
                "No test cases (.in files) found in '{}'",
                problem_dir.join(&problem_config.tests.dir).display()
            ),
            Some("Check problem.toml [tests.dir] configuration"),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let sandbox_mode_str = cli.sandbox_mode_str();

    if cli.require_tier
        && let Err(e) = uj_sandbox::create_sandbox(sandbox_mode_str, true)
    {
        let err_item = JudgeJsonItem::JudgeFailed {
            schema_version: 1,
            problem: problem_config.name.clone(),
            submission: String::new(),
            verdict: "SE",
            score: 0.0,
            max_time_ms: 0,
            max_memory_kb: 0,
            subtasks: Vec::new(),
            tests: Vec::new(),
            judge_failed_error: format!("Sandbox tier requirement not met: {e}"),
        };
        match format {
            OutputFormat::Json => {
                let stdout = std::io::stdout();
                let mut out = BufWriter::with_capacity(8 * 1024, stdout.lock());
                let _ = serde_json::to_writer_pretty(&mut out, &err_item);
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            OutputFormat::Ndjson => {
                println!("{}", serde_json::to_string(&err_item).unwrap());
            }
            OutputFormat::Human | OutputFormat::Silent => {
                eprintln!("Failed to satisfy sandbox tier requirement: {e}");
            }
        }
        return ExitCode::from(EXIT_JUDGE_FAILED);
    }

    let mut expanded_sources = Vec::new();
    for s in sources {
        let s_str = s.to_string_lossy();
        if s_str.contains('*') || s_str.contains('?') {
            let parent = s.parent().unwrap_or_else(|| Path::new("."));
            let pattern = s.file_name().unwrap_or_default().to_string_lossy();
            if let Ok(entries) = fs::read_dir(parent) {
                let mut matched = Vec::new();
                let glob = globset::Glob::new(&pattern)
                    .ok()
                    .map(|g| g.compile_matcher());
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(ref matcher) = glob
                        && matcher.is_match(entry.file_name())
                    {
                        matched.push(path);
                    }
                }
                matched.sort();
                if !matched.is_empty() {
                    expanded_sources.extend(matched);
                    continue;
                }
            }
        }
        expanded_sources.push(s.clone());
    }

    let use_tui = cli.tui && format == OutputFormat::Human && std::io::stderr().is_terminal();
    let compile_session = uj_engine::compiler::CompileSession::new();
    let mut overall_exit = ExitCode::from(EXIT_SUCCESS);
    let mut typed_json_reports: Vec<JudgeJsonItem> = Vec::with_capacity(expanded_sources.len());

    // Multi-submission streaming fast path: share `ProblemContext`, compile concurrently on
    // spare cores, and overlap test execution across submissions via `JudgePool::run_streaming`.
    if expanded_sources.len() > 1 {
        return run_multi_judge(
            &expanded_sources,
            problem_dir,
            &problem_config,
            &tests,
            cli,
            format,
            use_tui,
            &compile_session,
            cancel,
        );
    }

    let judge_options = JudgeOptions {
        sandbox_mode: sandbox_mode_str.to_string(),
        require_tier: cli.require_tier,
        fail_fast: cli.fail_fast,
        precise: cli.effective_precise(),
    };
    let scheduler = Scheduler::with_session(judge_options, compile_session.clone());

    for source in &expanded_sources {
        if cancel.is_cancelled() {
            return emit_interrupted(format, "Judging");
        }

        let source_name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
        let mem_limit = problem_config
            .to_limits()
            .map(|l| l.memory_bytes)
            .unwrap_or(512 * 1024 * 1024);
        let slot_count = Topology::detect(mem_limit, cli.jobs)
            .slots
            .len()
            .min(test_names.len());

        // Set up event streaming: TUI, NDJSON, or indicatif progress bar
        let (event_sender, event_thread) = if use_tui {
            let (tx, rx) = unbounded::<JudgeEvent>();
            let prob_name = problem_config.name.clone();
            let sub_name = source_name.clone();
            let cancel_clone = cancel.clone();

            let handle = thread::spawn(move || {
                if let Err(e) = uj_tui::run_tui(
                    &prob_name,
                    &sub_name,
                    &test_names,
                    slot_count,
                    rx.clone(),
                    cancel_clone,
                    true,
                ) {
                    eprintln!(
                        "Warning: TUI initialization failed ({e}); falling back to non-TUI execution."
                    );
                    while rx.recv().is_ok() {}
                }
            });
            (Some(tx), Some(handle))
        } else if format == OutputFormat::Ndjson {
            let (tx, handle) = spawn_buffered_ndjson_writer();
            (Some(tx), Some(handle))
        } else if format == OutputFormat::Human {
            let (tx, rx) = unbounded::<JudgeEvent>();
            let total_cnt = tests.len() as u64;
            let handle = thread::spawn(move || {
                let pb = indicatif::ProgressBar::new(total_cnt);
                pb.set_style(
                    indicatif::ProgressStyle::default_bar()
                        .template("{spinner:.green} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                        .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
                        .progress_chars("=>-"),
                );
                for ev in rx {
                    match ev {
                        JudgeEvent::TestDone(ref res) => {
                            pb.inc(1);
                            pb.set_message(format!("{}: {}", res.id, res.verdict.short_code()));
                        }
                        JudgeEvent::SubmissionDone(_) => {
                            break;
                        }
                        _ => {}
                    }
                }
                pb.finish_and_clear();
            });
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        if let Some(ref tx) = event_sender {
            let _ = tx.send(JudgeEvent::CompileStart {
                target: source.to_string_lossy().to_string(),
                compiler: "auto".to_string(),
            });
        }

        // 1. Compile source (or resolve output directory for output-only)
        let compile_res = compile_session.compile_problem_solution(
            problem_dir,
            &problem_config,
            source,
            cli.no_cache,
        );

        let compiled = match compile_res {
            Ok(res) => {
                if let Some(ref tx) = event_sender {
                    let _ = tx.send(JudgeEvent::CompileDone {
                        target: source.to_string_lossy().to_string(),
                        success: true,
                        duration_ms: res.duration_ms,
                        error_output: None,
                    });
                }
                res
            }
            Err(e) => {
                if let Some(ref tx) = event_sender {
                    let _ = tx.send(JudgeEvent::CompileDone {
                        target: source.to_string_lossy().to_string(),
                        success: false,
                        duration_ms: 0,
                        error_output: Some(e.to_string()),
                    });
                    let _ = tx.send(JudgeEvent::Summary {
                        total_time_ms: 0,
                        peak_memory_kb: 0,
                        final_verdict: Verdict::CompileError,
                        final_score: 0.0,
                    });
                }
                drop(event_sender);
                if let Some(h) = event_thread {
                    let _ = h.join();
                }

                let err_msg = match &e {
                    CompileError::Failed(msg) => msg.clone(),
                    other => other.to_string(),
                };
                let ce_struct = build_ce_struct(&problem_config.name, &source_name, &err_msg);
                let _ = save_run_report(problem_dir, &ce_struct);

                if format == OutputFormat::Json {
                    typed_json_reports.push(JudgeJsonItem::CompileError {
                        schema_version: 1,
                        problem: problem_config.name.clone(),
                        submission: source_name,
                        verdict: "CE",
                        score: 0.0,
                        max_time_ms: 0,
                        max_memory_kb: 0,
                        subtasks: Vec::new(),
                        tests: ce_struct.tests,
                        compile_error: err_msg,
                    });
                } else if format == OutputFormat::Human {
                    eprintln!("=== Compilation Error: {} ===", source.display());
                    eprintln!("{e}");
                }

                if overall_exit == ExitCode::from(EXIT_SUCCESS) {
                    overall_exit = ExitCode::from(EXIT_COMPILE_ERROR);
                }
                continue;
            }
        };

        // 2. Judge tests
        let judge_res = scheduler.judge_submission(
            problem_dir,
            &problem_config,
            &compiled,
            &source_name,
            tests.clone(),
            cli.jobs,
            event_sender,
            cancel,
        );

        if let Some(h) = event_thread {
            let _ = h.join();
        }

        if cancel.is_cancelled() {
            return emit_interrupted(format, "Judging");
        }

        match judge_res {
            Ok(report) => {
                let _ = save_run_report(problem_dir, &report);

                let code = match report.verdict {
                    Verdict::Accepted => EXIT_SUCCESS,
                    Verdict::JudgeFailed => EXIT_JUDGE_FAILED,
                    Verdict::CompileError => EXIT_COMPILE_ERROR,
                    _ => EXIT_NON_AC,
                };

                match format {
                    OutputFormat::Json => {
                        typed_json_reports.push(JudgeJsonItem::Report(report));
                    }
                    OutputFormat::Human => {
                        println!("{}", format_human_report(&report));
                    }
                    OutputFormat::Ndjson | OutputFormat::Silent => {
                        // NDJSON events were already streamed to stdout; Silent emits nothing
                    }
                }

                if code != EXIT_SUCCESS {
                    overall_exit = ExitCode::from(code);
                }
            }
            Err(e) => {
                let err_item = JudgeJsonItem::JudgeFailed {
                    schema_version: 1,
                    problem: problem_config.name.clone(),
                    submission: source_name,
                    verdict: "SE",
                    score: 0.0,
                    max_time_ms: 0,
                    max_memory_kb: 0,
                    subtasks: Vec::new(),
                    tests: Vec::new(),
                    judge_failed_error: e.to_string(),
                };

                if format == OutputFormat::Json {
                    typed_json_reports.push(err_item);
                } else if format == OutputFormat::Ndjson {
                    println!("{}", serde_json::to_string(&err_item).unwrap());
                } else if format == OutputFormat::Human {
                    eprintln!("Judging failed: {e}");
                }
                overall_exit = ExitCode::from(EXIT_JUDGE_FAILED);
            }
        }
    }

    if format == OutputFormat::Json {
        write_json_reports(&typed_json_reports);
    }

    overall_exit
}

fn write_json_reports(items: &[JudgeJsonItem]) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
    if items.len() == 1 {
        let _ = serde_json::to_writer_pretty(&mut out, &items[0]);
    } else {
        let _ = serde_json::to_writer_pretty(&mut out, items);
    }
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

#[allow(clippy::too_many_arguments)]
fn run_multi_judge(
    expanded_sources: &[PathBuf],
    problem_dir: &Path,
    problem_config: &uj_core::config::ProblemConfig,
    tests: &[uj_core::tests::TestCase],
    cli: &Cli,
    format: OutputFormat,
    use_tui: bool,
    compile_session: &uj_engine::compiler::CompileSession,
    cancel: &CancelToken,
) -> ExitCode {
    let sandbox_mode_str = cli.sandbox_mode_str();
    let p_ctx = match ProblemContext::prepare(0, problem_dir, problem_config, compile_session) {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            let err_str = e.to_string();
            let mut items = Vec::with_capacity(expanded_sources.len());
            for source in expanded_sources {
                let source_name = source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let err_item = JudgeJsonItem::JudgeFailed {
                    schema_version: 1,
                    problem: problem_config.name.clone(),
                    submission: source_name,
                    verdict: "SE",
                    score: 0.0,
                    max_time_ms: 0,
                    max_memory_kb: 0,
                    subtasks: Vec::new(),
                    tests: Vec::new(),
                    judge_failed_error: err_str.clone(),
                };
                if format == OutputFormat::Json {
                    items.push(err_item);
                } else if format == OutputFormat::Ndjson {
                    println!("{}", serde_json::to_string(&err_item).unwrap());
                } else if format == OutputFormat::Human {
                    eprintln!("Judging failed: {err_str}");
                }
            }
            if format == OutputFormat::Json {
                write_json_reports(&items);
            }
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let mem_limit = problem_config
        .to_limits()
        .map(|l| l.memory_bytes)
        .unwrap_or(512 * 1024 * 1024);
    let topology = Topology::detect(mem_limit, cli.jobs);
    let slots = topology.slots.len().max(1);
    let slot_cpus: Vec<Option<usize>> = topology.slots.iter().map(|s| s.cpu_core).collect();
    let spare_cores = topology.logical_cores.saturating_sub(slots);
    let compile_threads = if spare_cores >= 1 {
        spare_cores.clamp(1, 8)
    } else {
        (topology.logical_cores / 2).clamp(1, 4)
    }
    .min(expanded_sources.len());

    let (ndjson_tx, ndjson_handle) = if format == OutputFormat::Ndjson {
        let (tx, h) = spawn_buffered_ndjson_writer();
        (Some(tx), Some(h))
    } else {
        (None, None)
    };

    let (pool_event_tx, pool_event_rx) = unbounded::<PoolEvent>();
    let pool_events_opt = if use_tui || format == OutputFormat::Human {
        Some(pool_event_tx)
    } else {
        None
    };

    let total_tests = (expanded_sources.len() * tests.len()) as u64;
    let pb = if format == OutputFormat::Human && !use_tui {
        let p = indicatif::ProgressBar::new(total_tests);
        p.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        p.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
                .progress_chars("=>-"),
        );
        Some(p)
    } else {
        None
    };

    let human_progress_handle = if format == OutputFormat::Human && !use_tui {
        let pb_clone = pb.clone();
        let rx = pool_event_rx.clone();
        Some(thread::spawn(move || {
            while let Ok(pe) = rx.recv() {
                if let JudgeEvent::TestDone(ref res) = pe.inner
                    && let Some(ref p) = pb_clone
                {
                    p.inc(1);
                    p.set_message(format!(
                        "{} #{}: {}",
                        pe.submission.name,
                        res.id,
                        res.verdict.short_code()
                    ));
                }
            }
            if let Some(ref p) = pb_clone {
                p.finish_and_clear();
            }
        }))
    } else {
        None
    };

    let tui_handle = if use_tui {
        let test_names: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
        let sub_metas: Vec<SubmissionMeta> = expanded_sources
            .iter()
            .enumerate()
            .map(|(idx, s)| SubmissionMeta {
                id: idx,
                problem: problem_config.name.clone(),
                name: s
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                test_names: test_names.clone(),
            })
            .collect();
        let title = format!("Judge: {}", problem_config.name);
        let cancel_tui = cancel.clone();
        let rx = pool_event_rx.clone();
        Some(thread::spawn(move || {
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
                    "Warning: Multi-judge TUI initialization failed ({e}); falling back to non-TUI execution."
                );
                while rx.recv().is_ok() {}
            }
        }))
    } else {
        None
    };

    enum EarlyOutcome {
        CompileError {
            sub_id: usize,
            source: PathBuf,
            source_name: String,
            err_display: String,
            err_msg: String,
            ce_struct: SubmissionJudgeReport,
        },
        Failed {
            sub_id: usize,
            source_name: String,
            err_msg: String,
        },
    }

    #[allow(clippy::large_enum_variant)]
    enum PreparedSub {
        Ready(PoolSubmission),
        Early(EarlyOutcome),
    }

    let (work_tx, work_rx) = unbounded::<(usize, PathBuf)>();
    let (prep_tx, prep_rx) = unbounded::<PreparedSub>();
    let (stream_tx, stream_rx) = unbounded::<PoolSubmission>();

    let mut compile_workers = Vec::with_capacity(compile_threads);
    for _ in 0..compile_threads {
        let rx = work_rx.clone();
        let tx = prep_tx.clone();
        let session = compile_session.clone();
        let p_ctx = Arc::clone(&p_ctx);
        let tests_vec = tests.to_vec();
        let no_cache = cli.no_cache;
        let fail_fast = cli.fail_fast;
        let precise = cli.effective_precise();
        let ndjson_tx_worker = ndjson_tx.clone();
        let cancel_worker = cancel.clone();

        compile_workers.push(thread::spawn(move || {
            while let Ok((sub_id, source)) = rx.recv() {
                if cancel_worker.is_cancelled() {
                    break;
                }
                let source_name = source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let target_str = source.to_string_lossy().to_string();

                if let Some(ref ntx) = ndjson_tx_worker {
                    let _ = ntx.send(JudgeEvent::CompileStart {
                        target: target_str.clone(),
                        compiler: "auto".to_string(),
                    });
                }

                match session.compile_problem_solution(
                    &p_ctx.problem_dir,
                    &p_ctx.config,
                    &source,
                    no_cache,
                ) {
                    Ok(compiled) => {
                        if let Some(ref ntx) = ndjson_tx_worker {
                            let _ = ntx.send(JudgeEvent::CompileDone {
                                target: target_str,
                                success: true,
                                duration_ms: compiled.duration_ms,
                                error_output: None,
                            });
                        }
                        match SubmissionContext::prepare(
                            sub_id,
                            &source_name,
                            Arc::clone(&p_ctx),
                            &compiled,
                            tests_vec.clone(),
                            fail_fast,
                        ) {
                            Ok(sub_ctx) => {
                                let _ = tx.send(PreparedSub::Ready(PoolSubmission {
                                    ctx: Arc::new(sub_ctx),
                                    events: ndjson_tx_worker.clone(),
                                    precise,
                                }));
                            }
                            Err(e) => {
                                let _ = tx.send(PreparedSub::Early(EarlyOutcome::Failed {
                                    sub_id,
                                    source_name,
                                    err_msg: e.to_string(),
                                }));
                            }
                        }
                    }
                    Err(e) => {
                        let err_display = e.to_string();
                        if let Some(ref ntx) = ndjson_tx_worker {
                            let _ = ntx.send(JudgeEvent::CompileDone {
                                target: target_str,
                                success: false,
                                duration_ms: 0,
                                error_output: Some(err_display.clone()),
                            });
                            let _ = ntx.send(JudgeEvent::Summary {
                                total_time_ms: 0,
                                peak_memory_kb: 0,
                                final_verdict: Verdict::CompileError,
                                final_score: 0.0,
                            });
                        }
                        let err_msg = match &e {
                            CompileError::Failed(msg) => msg.clone(),
                            other => other.to_string(),
                        };
                        let ce_struct = build_ce_struct(&p_ctx.config.name, &source_name, &err_msg);
                        let _ = tx.send(PreparedSub::Early(EarlyOutcome::CompileError {
                            sub_id,
                            source,
                            source_name,
                            err_display,
                            err_msg,
                            ce_struct,
                        }));
                    }
                }
            }
        }));
    }

    drop(prep_tx);
    for (idx, src) in expanded_sources.iter().cloned().enumerate() {
        let _ = work_tx.send((idx, src));
    }
    drop(work_tx);

    let router_handle = {
        let pb_router = pb.clone();
        let router_pool_events = pool_events_opt.clone();
        let prob_name = problem_config.name.clone();
        let tests_per_sub = tests.len() as u64;
        thread::spawn(move || {
            let mut early_map: HashMap<usize, EarlyOutcome> = HashMap::new();
            let mut cur_len = total_tests;
            while let Ok(item) = prep_rx.recv() {
                match item {
                    PreparedSub::Ready(pool_sub) => {
                        let _ = stream_tx.send(pool_sub);
                    }
                    PreparedSub::Early(early) => {
                        if let Some(ref p) = pb_router {
                            cur_len = cur_len.saturating_sub(tests_per_sub);
                            p.set_length(cur_len);
                        }
                        match &early {
                            EarlyOutcome::CompileError {
                                sub_id,
                                source_name,
                                err_display,
                                ce_struct,
                                ..
                            } => {
                                if use_tui && let Some(ref ptx) = router_pool_events {
                                    let _ = ptx.send(PoolEvent {
                                        submission: SubmissionRef {
                                            id: *sub_id,
                                            problem: prob_name.clone(),
                                            name: source_name.clone(),
                                        },
                                        inner: JudgeEvent::CompileDone {
                                            target: source_name.clone(),
                                            success: false,
                                            duration_ms: 0,
                                            error_output: Some(err_display.clone()),
                                        },
                                    });
                                    let _ = ptx.send(PoolEvent {
                                        submission: SubmissionRef {
                                            id: *sub_id,
                                            problem: prob_name.clone(),
                                            name: source_name.clone(),
                                        },
                                        inner: JudgeEvent::SubmissionDone(ce_struct.clone()),
                                    });
                                }
                                early_map.insert(*sub_id, early);
                            }
                            EarlyOutcome::Failed {
                                sub_id,
                                source_name,
                                err_msg,
                            } => {
                                if use_tui && let Some(ref ptx) = router_pool_events {
                                    let fail_report = SubmissionJudgeReport {
                                        schema_version: 1,
                                        problem: prob_name.clone(),
                                        submission: source_name.clone(),
                                        verdict: Verdict::JudgeFailed,
                                        score: 0.0,
                                        max_time_ms: 0,
                                        max_memory_kb: 0,
                                        subtasks: vec![],
                                        tests: vec![SingleTestResult {
                                            id: "setup".to_string(),
                                            subtask: None,
                                            verdict: Verdict::JudgeFailed,
                                            time_ms: 0,
                                            wall_ms: 0,
                                            memory_kb: 0,
                                            score: 0.0,
                                            exit: uj_report::result::ExitInfo {
                                                code: None,
                                                signal: None,
                                                nt_status: None,
                                            },
                                            message: Some(err_msg.clone()),
                                            checker: None,
                                            diff: None,
                                            timing: uj_report::result::TimingInfo::default(),
                                            sandbox_tier: None,
                                        }],
                                    };
                                    let _ = ptx.send(PoolEvent {
                                        submission: SubmissionRef {
                                            id: *sub_id,
                                            problem: prob_name.clone(),
                                            name: source_name.clone(),
                                        },
                                        inner: JudgeEvent::SubmissionDone(fail_report),
                                    });
                                }
                                early_map.insert(*sub_id, early);
                            }
                        }
                    }
                }
            }
            drop(stream_tx);
            early_map
        })
    };

    let mut pool_opts =
        PoolOptions::default_for(sandbox_mode_str.to_string(), cli.require_tier, cli.jobs);
    pool_opts.pool_events = pool_events_opt;

    let pool = JudgePool::new(pool_opts);
    let pool_results = pool.run_streaming(stream_rx, cancel);

    for h in compile_workers {
        let _ = h.join();
    }
    let mut early_map = router_handle.join().unwrap_or_default();

    drop(ndjson_tx);
    drop(pool_event_rx);

    if let Some(h) = human_progress_handle {
        let _ = h.join();
    }
    if let Some(h) = ndjson_handle {
        let _ = h.join();
    }
    if let Some(h) = tui_handle {
        let _ = h.join();
    }

    if cancel.is_cancelled() {
        return emit_interrupted(format, "Judging");
    }

    let mut pool_map: HashMap<usize, Result<SubmissionJudgeReport, _>> =
        pool_results.into_iter().collect();
    let mut overall_exit = ExitCode::from(EXIT_SUCCESS);
    let mut typed_json_reports: Vec<JudgeJsonItem> = Vec::with_capacity(expanded_sources.len());

    for (sub_id, source) in expanded_sources.iter().enumerate() {
        if let Some(early) = early_map.remove(&sub_id) {
            match early {
                EarlyOutcome::CompileError {
                    source: ce_src,
                    source_name,
                    err_display,
                    err_msg,
                    ce_struct,
                    ..
                } => {
                    let _ = save_run_report(problem_dir, &ce_struct);
                    if format == OutputFormat::Json {
                        typed_json_reports.push(JudgeJsonItem::CompileError {
                            schema_version: 1,
                            problem: problem_config.name.clone(),
                            submission: source_name,
                            verdict: "CE",
                            score: 0.0,
                            max_time_ms: 0,
                            max_memory_kb: 0,
                            subtasks: Vec::new(),
                            tests: ce_struct.tests,
                            compile_error: err_msg,
                        });
                    } else if format == OutputFormat::Human {
                        eprintln!("=== Compilation Error: {} ===", ce_src.display());
                        eprintln!("{err_display}");
                    }
                    if overall_exit == ExitCode::from(EXIT_SUCCESS) {
                        overall_exit = ExitCode::from(EXIT_COMPILE_ERROR);
                    }
                }
                EarlyOutcome::Failed {
                    source_name,
                    err_msg,
                    ..
                } => {
                    let err_item = JudgeJsonItem::JudgeFailed {
                        schema_version: 1,
                        problem: problem_config.name.clone(),
                        submission: source_name,
                        verdict: "SE",
                        score: 0.0,
                        max_time_ms: 0,
                        max_memory_kb: 0,
                        subtasks: Vec::new(),
                        tests: Vec::new(),
                        judge_failed_error: err_msg.clone(),
                    };
                    if format == OutputFormat::Json {
                        typed_json_reports.push(err_item);
                    } else if format == OutputFormat::Ndjson {
                        println!("{}", serde_json::to_string(&err_item).unwrap());
                    } else if format == OutputFormat::Human {
                        eprintln!("Judging failed: {err_msg}");
                    }
                    overall_exit = ExitCode::from(EXIT_JUDGE_FAILED);
                }
            }
            continue;
        }

        if let Some(judge_res) = pool_map.remove(&sub_id) {
            match judge_res {
                Ok(report) => {
                    let _ = save_run_report(problem_dir, &report);
                    let code = match report.verdict {
                        Verdict::Accepted => EXIT_SUCCESS,
                        Verdict::JudgeFailed => EXIT_JUDGE_FAILED,
                        Verdict::CompileError => EXIT_COMPILE_ERROR,
                        _ => EXIT_NON_AC,
                    };
                    match format {
                        OutputFormat::Json => {
                            typed_json_reports.push(JudgeJsonItem::Report(report));
                        }
                        OutputFormat::Human => {
                            println!("{}", format_human_report(&report));
                        }
                        OutputFormat::Ndjson | OutputFormat::Silent => {}
                    }
                    if code != EXIT_SUCCESS {
                        overall_exit = ExitCode::from(code);
                    }
                }
                Err(e) => {
                    let source_name = source
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let err_item = JudgeJsonItem::JudgeFailed {
                        schema_version: 1,
                        problem: problem_config.name.clone(),
                        submission: source_name,
                        verdict: "SE",
                        score: 0.0,
                        max_time_ms: 0,
                        max_memory_kb: 0,
                        subtasks: Vec::new(),
                        tests: Vec::new(),
                        judge_failed_error: e.to_string(),
                    };
                    if format == OutputFormat::Json {
                        typed_json_reports.push(err_item);
                    } else if format == OutputFormat::Ndjson {
                        println!("{}", serde_json::to_string(&err_item).unwrap());
                    } else if format == OutputFormat::Human {
                        eprintln!("Judging failed: {e}");
                    }
                    overall_exit = ExitCode::from(EXIT_JUDGE_FAILED);
                }
            }
        }
    }

    if format == OutputFormat::Json {
        write_json_reports(&typed_json_reports);
    }

    overall_exit
}

pub fn run_last(problem_dir: &Path, format: OutputFormat) -> ExitCode {
    let last_path = problem_dir.join(".uj").join("runs").join("last.json");
    if !last_path.exists() {
        emit_error(
            format,
            "ReportNotFound",
            format!(
                "No previous run found at '{}'. Run 'uj judge' first.",
                last_path.display()
            ),
            Some("Run 'uj judge' first to generate a run report."),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let content = match fs::read_to_string(&last_path) {
        Ok(c) => c,
        Err(e) => {
            emit_error(
                format,
                "ReportReadError",
                format!("Failed to read last run '{}': {e}", last_path.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let report: SubmissionJudgeReport = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            emit_error(
                format,
                "ReportParseError",
                format!("Failed to parse last run '{}': {e}", last_path.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(&report).unwrap());
        }
        OutputFormat::Human => {
            println!("{}", format_human_report(&report));
        }
        OutputFormat::Silent => {}
    }

    if report.verdict == Verdict::Accepted {
        ExitCode::from(EXIT_SUCCESS)
    } else {
        ExitCode::from(EXIT_NON_AC)
    }
}

pub fn run_show(
    problem_dir: &Path,
    run_id: Option<&str>,
    test_id: Option<&str>,
    format: OutputFormat,
) -> ExitCode {
    let run_file = if let Some(id) = run_id {
        let valid_id = match validate_run_id(id) {
            Ok(s) => s,
            Err(msg) => {
                emit_error(
                    format,
                    "InvalidRunId",
                    msg,
                    Some("Provide a safe run ID without path separators or '..'."),
                );
                return ExitCode::from(EXIT_USAGE_OR_CONFIG);
            }
        };
        problem_dir
            .join(".uj")
            .join("runs")
            .join(format!("{valid_id}.json"))
    } else {
        problem_dir.join(".uj").join("runs").join("last.json")
    };

    if !run_file.exists() {
        emit_error(
            format,
            "ReportNotFound",
            format!("Run file '{}' does not exist.", run_file.display()),
            Some("Run 'uj judge' first to generate a run report."),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let content = match fs::read_to_string(&run_file) {
        Ok(c) => c,
        Err(e) => {
            emit_error(
                format,
                "ReportReadError",
                format!("Failed to read run file '{}': {e}", run_file.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let report: SubmissionJudgeReport = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            emit_error(
                format,
                "ReportParseError",
                format!("Failed to parse run report '{}': {e}", run_file.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    if let Some(target_test) = test_id {
        if let Some(test_res) = report.tests.iter().find(|t| t.id == target_test) {
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(test_res).unwrap());
                }
                OutputFormat::Ndjson => {
                    println!("{}", serde_json::to_string(test_res).unwrap());
                }
                OutputFormat::Human => {
                    println!("{}", format_test_details(test_res));
                }
                OutputFormat::Silent => {}
            }
            if test_res.verdict == Verdict::Accepted {
                ExitCode::from(EXIT_SUCCESS)
            } else {
                ExitCode::from(EXIT_NON_AC)
            }
        } else {
            emit_error(
                format,
                "TestNotFound",
                format!(
                    "Test case '{target_test}' not found in run '{}'.",
                    run_file.display()
                ),
                None,
            );
            ExitCode::from(EXIT_USAGE_OR_CONFIG)
        }
    } else {
        match format {
            OutputFormat::Json => {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            }
            OutputFormat::Ndjson => {
                println!("{}", serde_json::to_string(&report).unwrap());
            }
            OutputFormat::Human => {
                println!("{}", format_human_report(&report));
            }
            OutputFormat::Silent => {}
        }
        if report.verdict == Verdict::Accepted {
            ExitCode::from(EXIT_SUCCESS)
        } else {
            ExitCode::from(EXIT_NON_AC)
        }
    }
}

pub fn run_clean(problem_dir: &Path, format: OutputFormat) -> ExitCode {
    uj_engine::clear_answer_cache();
    let uj_dir = problem_dir.join(".uj");
    let existed = uj_dir.exists();
    if existed {
        let cache_dir = uj_dir.join("cache");
        let runs_dir = uj_dir.join("runs");
        let tmp_dir = uj_dir.join("tmp");

        let _ = fs::remove_dir_all(&cache_dir);
        let _ = fs::remove_dir_all(&runs_dir);
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    let res = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "cleaned": existed,
        "path": uj_dir.to_string_lossy(),
    });

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        }
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(&res).unwrap());
        }
        OutputFormat::Human => {
            if existed {
                println!("Cache and run records in '{}' cleaned.", uj_dir.display());
            } else {
                println!("No .uj directory found; nothing to clean.");
            }
        }
        OutputFormat::Silent => {}
    }

    ExitCode::from(EXIT_SUCCESS)
}

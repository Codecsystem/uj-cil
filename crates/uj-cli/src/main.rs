mod author_cmd;
mod cli;
mod contest_cmd;
mod doctor;
mod error;
mod judge_cmd;
mod lemon_import;
mod mcp_server;
mod report_cmd;

use clap::Parser;
use std::io::IsTerminal;
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use cli::{Cli, Commands, OutputFormat, SandboxMode, SchemaTarget};
use error::{EXIT_INTERRUPTED, EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG};

fn init_logging(verbose: u8, quiet: bool) {
    if quiet {
        return;
    }

    let default_level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    let filter =
        EnvFilter::try_from_env("UJ_LOG").unwrap_or_else(|_| EnvFilter::new(default_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init();
}

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("UJ_IN_SYSTEMD_SCOPE").is_none()
            && let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup")
        {
            let trimmed = cgroup.trim();
            if (trimmed.ends_with("/init.scope") || !trimmed.contains("user@"))
                && let Ok(exe) = std::env::current_exe()
            {
                let mut cmd = std::process::Command::new("systemd-run");
                cmd.arg("--user")
                    .arg("--scope")
                    .arg("--quiet")
                    .arg("-p")
                    .arg("Delegate=yes")
                    .arg("-E")
                    .arg("UJ_IN_SYSTEMD_SCOPE=1")
                    .arg("--")
                    .arg(&exe)
                    .args(std::env::args_os().skip(1));
                if let Ok(mut child) = cmd.spawn() {
                    let status = child.wait().unwrap_or_default();
                    std::process::exit(status.code().unwrap_or(1));
                }
            }
        }
    }

    let args = Cli::parse();
    init_logging(args.verbose, args.quiet);

    let format = args.format.unwrap_or_else(|| {
        if std::io::stdout().is_terminal() {
            OutputFormat::Human
        } else {
            OutputFormat::Json
        }
    });

    match &args.command {
        Commands::Schema { target } => {
            let schema_str = match *target {
                SchemaTarget::Result => uj_report::schema::generate_result_schema(),
                SchemaTarget::Config => uj_report::schema::generate_config_schema(),
                SchemaTarget::Events => uj_report::schema::generate_events_schema(),
            };
            println!("{schema_str}");
            ExitCode::from(EXIT_SUCCESS)
        }
        Commands::Doctor {
            problem,
            fix_apparmor,
        } => {
            let report = doctor::run_doctor(*problem, *fix_apparmor);
            if format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!("=== uj doctor report ===");
                println!("Platform: {}", report.platform);
                println!();
                for check in &report.checks {
                    let symbol = match check.status {
                        doctor::CheckStatus::Pass => "[OK]",
                        doctor::CheckStatus::Warn => "[WARN]",
                        doctor::CheckStatus::Fail => "[FAIL]",
                    };
                    println!("{symbol} {}: {}", check.name, check.message);
                    if let Some(rec) = &check.recommendation {
                        println!("     -> Recommendation: {rec}");
                    }
                }
                println!();
                if report.ok {
                    println!("Result: All critical checks passed.");
                } else {
                    println!("Result: One or more critical checks failed.");
                }
            }
            if report.ok {
                ExitCode::from(EXIT_SUCCESS)
            } else {
                ExitCode::from(EXIT_JUDGE_FAILED)
            }
        }
        Commands::Run { cmd } => {
            let sandbox_mode_str = match args.sandbox {
                SandboxMode::Auto => "auto",
                SandboxMode::Full => "full",
                SandboxMode::Degraded => "degraded",
                SandboxMode::None => "none",
            };

            let sandbox = match uj_sandbox::create_sandbox(sandbox_mode_str, args.require_tier) {
                Ok(sb) => sb,
                Err(e) => {
                    if format == OutputFormat::Json {
                        let err_obj = serde_json::json!({
                            "ok": false,
                            "error": {
                                "kind": "SandboxError",
                                "message": e.to_string(),
                            }
                        });
                        println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
                    } else {
                        eprintln!("Failed to initialize sandbox: {e}");
                    }
                    return ExitCode::from(EXIT_JUDGE_FAILED);
                }
            };

            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });

            let spec = uj_core::run::RunSpec {
                argv: cmd.iter().map(std::ffi::OsString::from).collect(),
                env: vec![],
                work_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                ro_binds: vec![],
                stdin: uj_core::run::Stdio::Null,
                stdout: uj_core::run::Stdio::Pipe,
                stderr: uj_core::run::Stdio::Pipe,
                extra_fds: vec![],
                limits: uj_core::run::Limits::new(5000, 512 * 1024 * 1024),
                cpu_affinity: None,
                seccomp_profile: Default::default(),
                trusted: false,
            };

            match sandbox.run(&spec, &cancel) {
                Ok(res) => {
                    if cancel.is_cancelled() {
                        return ExitCode::from(EXIT_INTERRUPTED);
                    }
                    if format == OutputFormat::Json {
                        println!("{}", serde_json::to_string_pretty(&res).unwrap());
                    } else {
                        println!("Status:       {:?}", res.status);
                        if let Some(reason) = res.killed_by {
                            println!("Killed By:    {:?}", reason);
                        }
                        println!("CPU Time:     {:.3} ms", res.cpu_time_us as f64 / 1000.0);
                        println!("Wall Time:    {:.3} ms", res.wall_time_us as f64 / 1000.0);
                        println!(
                            "Peak Memory:  {:.2} MiB",
                            res.memory_peak_bytes as f64 / (1024.0 * 1024.0)
                        );
                        println!("Sandbox Tier: {:?}", res.sandbox_tier);
                        if let Some(stdout) = res.stdout
                            && !stdout.bytes.is_empty()
                        {
                            println!("--- STDOUT ---");
                            print!("{}", String::from_utf8_lossy(&stdout.bytes));
                            if !stdout.bytes.ends_with(b"\n") {
                                println!();
                            }
                        }
                        if !res.stderr_tail.is_empty() {
                            println!("--- STDERR ---");
                            print!("{}", String::from_utf8_lossy(&res.stderr_tail));
                            if !res.stderr_tail.ends_with(b"\n") {
                                println!();
                            }
                        }
                    }

                    if res.status.is_success() && res.killed_by.is_none() {
                        ExitCode::from(EXIT_SUCCESS)
                    } else {
                        ExitCode::from(EXIT_NON_AC)
                    }
                }
                Err(e) => {
                    if format == OutputFormat::Json {
                        let err_obj = serde_json::json!({
                            "ok": false,
                            "error": {
                                "kind": "JudgeFailed",
                                "message": e.to_string(),
                            }
                        });
                        println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
                    } else {
                        eprintln!("Execution failed: {e}");
                    }
                    ExitCode::from(EXIT_JUDGE_FAILED)
                }
            }
        }
        Commands::Judge { sources, problem } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            judge_cmd::run_judge(sources, problem, &args, format, &cancel)
        }
        Commands::Last => judge_cmd::run_last(std::path::Path::new("."), format),
        Commands::Show { run_id, test } => judge_cmd::run_show(
            std::path::Path::new("."),
            run_id.as_deref(),
            test.as_deref(),
            format,
        ),
        Commands::Clean => judge_cmd::run_clean(std::path::Path::new(".")),
        Commands::Init { dir, problem_type } => author_cmd::run_init(dir, problem_type, format),
        Commands::Build { problem, targets } => {
            author_cmd::run_build(problem, targets, args.no_cache, format)
        }
        Commands::Gen { problem, force } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_gen(problem, *force, format, &cancel)
        }
        Commands::Validate { problem } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_validate(problem, format, &cancel)
        }
        Commands::Ans { problem, force } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_ans(problem, *force, format, &cancel)
        }
        Commands::Verify { problem } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_verify(problem, &args, format, &cancel)
        }
        Commands::Tl { problem, apply } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_tl(problem, *apply, &args, format, &cancel)
        }
        Commands::Stress {
            problem,
            generator,
            solution,
            brute,
            count,
            seed,
            time_budget,
        } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_stress(
                problem,
                generator,
                solution,
                brute,
                *count,
                *seed,
                time_budget,
                &args,
                format,
                &cancel,
            )
        }
        Commands::Pipeline { problem } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            author_cmd::run_pipeline(problem, &args, format, &cancel)
        }
        Commands::Report { run_id, html, csv } => report_cmd::run_report(
            std::path::Path::new("."),
            run_id.as_deref(),
            html.as_deref(),
            csv.as_deref(),
            format,
        ),
        Commands::Contest { dir } => {
            let cancel = uj_sandbox::CancelToken::new();
            let cancel_clone = cancel.clone();
            let _ = ctrlc::set_handler(move || {
                cancel_clone.cancel();
            });
            contest_cmd::run_contest(dir, &args, format, &cancel)
        }
        Commands::Import { kind, file } => {
            if kind == "lemon" {
                lemon_import::run_import_lemon(file, format)
            } else {
                eprintln!("Unsupported format '{kind}'. Supported formats: lemon");
                ExitCode::from(EXIT_USAGE_OR_CONFIG)
            }
        }
        Commands::Mcp => mcp_server::run_mcp_server(),
    }
}

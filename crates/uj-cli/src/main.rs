mod cli;
mod doctor;
mod error;

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
    let args = Cli::parse();
    init_logging(args.verbose, args.quiet);

    let format = args.format.unwrap_or_else(|| {
        if std::io::stdout().is_terminal() {
            OutputFormat::Human
        } else {
            OutputFormat::Json
        }
    });

    match args.command {
        Commands::Schema { target } => {
            let schema_str = match target {
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
            let report = doctor::run_doctor(problem, fix_apparmor);
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
                argv: cmd.into_iter().map(std::ffi::OsString::from).collect(),
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
        _ => {
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "Unimplemented",
                        "message": "Command not yet implemented in scaffolding phase (P0)",
                        "hint": "Check docs/PLAN.md for current phase progress"
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!(
                    "This command will be implemented in subsequent phases. See docs/PLAN.md."
                );
            }
            ExitCode::from(EXIT_USAGE_OR_CONFIG)
        }
    }
}

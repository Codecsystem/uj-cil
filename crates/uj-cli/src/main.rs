pub(crate) mod author_cache;
mod author_cmd;
pub(crate) mod author_steps;
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

use cli::{Cli, ColorMode, Commands, OutputFormat, SchemaTarget};
use error::{
    EXIT_JUDGE_FAILED, EXIT_NON_AC, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG, emit_error,
    emit_interrupted, install_ctrlc, print_structured,
};

fn apply_color_mode(color: ColorMode) -> bool {
    match color {
        ColorMode::Never => {
            // SAFETY: invoked at process startup before spawning worker threads.
            unsafe {
                std::env::set_var("NO_COLOR", "1");
            }
            false
        }
        ColorMode::Always => {
            // SAFETY: invoked at process startup before spawning worker threads.
            unsafe {
                std::env::set_var("CLICOLOR_FORCE", "1");
            }
            true
        }
        ColorMode::Auto => {
            std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        }
    }
}

fn init_logging(verbose: u8, quiet: bool, use_ansi: bool) {
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
        .with_ansi(use_ansi)
        .with_target(false);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init();
}

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::ExitStatusExt;

        let raw_args: Vec<String> = std::env::args().collect();
        let is_help_or_version = raw_args
            .iter()
            .any(|a| a == "-h" || a == "--help" || a == "-V" || a == "--version");
        let requires_sandbox_cmd = raw_args.iter().any(|a| {
            matches!(
                a.as_str(),
                "judge"
                    | "contest"
                    | "stress"
                    | "run"
                    | "pipeline"
                    | "verify"
                    | "tl"
                    | "gen"
                    | "validate"
                    | "ans"
                    | "mcp"
            )
        });
        let explicit_none = raw_args
            .windows(2)
            .any(|w| w[0] == "--sandbox" && w[1] == "none")
            || raw_args.iter().any(|a| a == "--sandbox=none");

        if !is_help_or_version
            && requires_sandbox_cmd
            && !explicit_none
            && unsafe { libc::getuid() } != 0
            && std::env::var_os("UJ_IN_SYSTEMD_SCOPE").is_none()
        {
            let in_delegated_scope =
                if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
                    let rel = cgroup
                        .lines()
                        .find_map(|l| l.strip_prefix("0::"))
                        .unwrap_or("")
                        .trim_start_matches('/');
                    let p = std::path::Path::new("/sys/fs/cgroup").join(rel);
                    p.file_name()
                        .map(|n| n.to_string_lossy().contains(".scope"))
                        .unwrap_or(false)
                        && p.join("cgroup.controllers").exists()
                        && std::fs::OpenOptions::new()
                            .write(true)
                            .open(p.join("cgroup.subtree_control"))
                            .is_ok()
                } else {
                    false
                };

            if !in_delegated_scope {
                let bus_available = std::process::Command::new("systemd-run")
                    .args(["--user", "--scope", "--quiet", "true"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);

                if bus_available && let Ok(exe) = std::env::current_exe() {
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
                        let exit_code = status
                            .code()
                            .or_else(|| status.signal().map(|sig| 128 + sig))
                            .unwrap_or(1);
                        std::process::exit(exit_code);
                    }
                }
            }
        }
    }

    let args = Cli::parse();
    let use_ansi = apply_color_mode(args.color);
    init_logging(args.verbose, args.quiet, use_ansi);

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
            let report = doctor::run_doctor(problem.as_deref(), *fix_apparmor);
            match format {
                OutputFormat::Json | OutputFormat::Ndjson => {
                    print_structured(format, &report);
                }
                OutputFormat::Human => {
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
                OutputFormat::Silent => {}
            }
            if report.ok {
                ExitCode::from(EXIT_SUCCESS)
            } else {
                ExitCode::from(EXIT_JUDGE_FAILED)
            }
        }
        Commands::Run { cmd } => {
            let sandbox_mode_str = args.sandbox_mode_str();

            let sandbox = match uj_sandbox::create_sandbox(sandbox_mode_str, args.require_tier) {
                Ok(sb) => sb,
                Err(e) => {
                    emit_error(
                        format,
                        "SandboxError",
                        format!("Failed to initialize sandbox: {e}"),
                        None,
                    );
                    return ExitCode::from(EXIT_JUDGE_FAILED);
                }
            };

            let cancel = install_ctrlc();

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
                        return emit_interrupted(format, "Execution");
                    }
                    match format {
                        OutputFormat::Json | OutputFormat::Ndjson => {
                            print_structured(format, &res);
                        }
                        OutputFormat::Human => {
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
                        OutputFormat::Silent => {}
                    }

                    if res.status.is_success() && res.killed_by.is_none() {
                        ExitCode::from(EXIT_SUCCESS)
                    } else {
                        ExitCode::from(EXIT_NON_AC)
                    }
                }
                Err(e) => {
                    emit_error(
                        format,
                        "JudgeFailed",
                        format!("Execution failed: {e}"),
                        None,
                    );
                    ExitCode::from(EXIT_JUDGE_FAILED)
                }
            }
        }
        Commands::Judge { sources, problem } => {
            let cancel = install_ctrlc();
            judge_cmd::run_judge(sources, problem, &args, format, &cancel)
        }
        Commands::Last { problem } => judge_cmd::run_last(problem, format),
        Commands::Show {
            problem,
            run_id,
            test,
        } => judge_cmd::run_show(problem, run_id.as_deref(), test.as_deref(), format),
        Commands::Clean { problem } => judge_cmd::run_clean(problem, format),
        Commands::Init {
            dir,
            problem_type,
            name,
            title,
            force,
        } => author_cmd::run_init(
            dir,
            problem_type.as_str(),
            name.as_deref(),
            title.as_deref(),
            *force,
            format,
        ),
        Commands::Build { problem, targets } => {
            author_cmd::run_build(problem, targets, args.no_cache, format)
        }
        Commands::Gen {
            problem,
            force,
            append,
        } => {
            let cancel = install_ctrlc();
            author_cmd::run_gen(problem, *force, *append, &args, format, &cancel)
        }
        Commands::Validate { problem } => {
            let cancel = install_ctrlc();
            author_cmd::run_validate(problem, &args, format, &cancel)
        }
        Commands::Ans { problem, force } => {
            let cancel = install_ctrlc();
            author_cmd::run_ans(problem, *force, &args, format, &cancel)
        }
        Commands::Verify { problem } => {
            let cancel = install_ctrlc();
            author_cmd::run_verify(problem, &args, format, &cancel)
        }
        Commands::Tl { problem, apply } => {
            let cancel = install_ctrlc();
            author_cmd::run_tl(problem, *apply, &args, format, &cancel)
        }
        Commands::Stress {
            problem,
            generator,
            solution,
            brute,
            count,
            seed,
            gen_args,
            time_budget,
        } => {
            let cancel = install_ctrlc();
            author_cmd::run_stress(
                problem,
                generator,
                solution,
                brute,
                *count,
                *seed,
                gen_args,
                *time_budget,
                &args,
                format,
                &cancel,
            )
        }
        Commands::Pack {
            problem,
            output,
            clean,
        } => author_cmd::run_pack(problem, output, *clean, format),
        Commands::Pipeline { problem } => {
            let cancel = install_ctrlc();
            author_cmd::run_pipeline(problem, &args, format, &cancel)
        }
        Commands::Report {
            problem,
            run_id,
            html,
            csv,
        } => report_cmd::run_report(
            problem,
            run_id.as_deref(),
            html.as_deref(),
            csv.as_deref(),
            format,
        ),
        Commands::Contest {
            dir,
            problems,
            contestants,
        } => {
            let cancel = install_ctrlc();
            contest_cmd::run_contest(dir, problems, contestants, &args, format, &cancel)
        }
        Commands::Import { kind, file, force } => {
            if kind == "lemon" {
                lemon_import::run_import_lemon(file, *force, format)
            } else {
                emit_error(
                    format,
                    "UnsupportedFormat",
                    format!("Unsupported format '{kind}'. Supported formats: lemon"),
                    None,
                );
                ExitCode::from(EXIT_USAGE_OR_CONFIG)
            }
        }
        Commands::Mcp => mcp_server::run_mcp_server(),
    }
}

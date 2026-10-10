mod cli;
mod error;

use clap::Parser;
use std::io::IsTerminal;
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use cli::{Cli, Commands, OutputFormat, SchemaTarget};
use error::{EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG};

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
        Commands::Doctor { .. } => {
            eprintln!("uj doctor: checking environment...");
            #[cfg(target_os = "linux")]
            eprintln!("Linux detected. Detailed checks in P1.");
            #[cfg(target_os = "windows")]
            eprintln!("Windows detected. Detailed checks in P2.");
            ExitCode::from(EXIT_SUCCESS)
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

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Ndjson,
    #[value(skip)]
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum SandboxMode {
    #[default]
    Auto,
    Full,
    Degraded,
    None,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Auto => "auto",
            SandboxMode::Full => "full",
            SandboxMode::Degraded => "degraded",
            SandboxMode::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum InitProblemType {
    #[default]
    Traditional,
    Interactive,
    Grader,
    Communication,
    #[value(name = "output_only", alias = "output-only")]
    OutputOnly,
}

impl InitProblemType {
    pub fn as_str(self) -> &'static str {
        match self {
            InitProblemType::Traditional => "traditional",
            InitProblemType::Interactive => "interactive",
            InitProblemType::Grader => "grader",
            InitProblemType::Communication => "communication",
            InitProblemType::OutputOnly => "output_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SchemaTarget {
    Result,
    Config,
    Events,
}

/// Parse a human-friendly duration string (`"500ms"`, `"10s"`, `"1.5m"`, `"2h"`, or plain seconds).
pub fn parse_duration_str(raw: &str) -> Result<Duration, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(
            "Duration cannot be empty (expected e.g. '500ms', '10s', '5m', '1h')".to_string(),
        );
    }

    let (num_str, unit) = if let Some(v) = s.strip_suffix("ms") {
        (v.trim(), "ms")
    } else if let Some(v) = s.strip_suffix('s') {
        (v.trim(), "s")
    } else if let Some(v) = s.strip_suffix('m') {
        (v.trim(), "m")
    } else if let Some(v) = s.strip_suffix('h') {
        (v.trim(), "h")
    } else {
        (s, "s")
    };

    if num_str.is_empty() {
        return Err(format!("Invalid duration '{raw}': missing numeric value"));
    }

    let val: f64 = num_str.parse().map_err(|_| {
        format!("Invalid duration '{raw}': expected a positive number followed by ms, s, m, or h")
    })?;

    if !val.is_finite() || val <= 0.0 {
        return Err(format!(
            "Invalid duration '{raw}': duration must be a finite positive number"
        ));
    }

    let secs = match unit {
        "ms" => val / 1000.0,
        "s" => val,
        "m" => val * 60.0,
        "h" => val * 3600.0,
        _ => unreachable!(),
    };

    if secs > (u64::MAX / 1000) as f64 {
        return Err(format!("Invalid duration '{raw}': value is too large"));
    }

    Ok(Duration::from_secs_f64(secs))
}

#[derive(Parser, Debug)]
#[command(
    name = "uj",
    author,
    version,
    about = "Fast, lightweight local competitive programming judge and problem development toolkit"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_enum,
        help = "Output format [default: human when TTY, json otherwise]"
    )]
    pub format: Option<OutputFormat>,

    #[arg(
        short = 'j',
        long = "jobs",
        global = true,
        help = "Number of parallel jobs/slots [default: physical cores - 1]"
    )]
    pub jobs: Option<usize>,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        help = "Sandbox isolation level"
    )]
    pub sandbox: SandboxMode,

    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Require full tier sandbox or abort"
    )]
    pub require_tier: bool,

    #[arg(
        long,
        global = true,
        default_value_t = true,
        num_args = 0..=1,
        default_missing_value = "true",
        action = clap::ArgAction::Set,
        help = "Enable precise rejudging for edge cases"
    )]
    pub precise: bool,

    #[arg(
        long = "no-precise",
        global = true,
        default_value_t = false,
        help = "Disable precise rejudging for edge cases"
    )]
    pub no_precise: bool,

    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Disable compilation and answer caches"
    )]
    pub no_cache: bool,

    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Stop subtask upon first failed test case"
    )]
    pub fail_fast: bool,

    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Enable Ratatui live monitoring TUI"
    )]
    pub tui: bool,

    #[arg(
        short = 'v',
        long = "verbose",
        global = true,
        action = clap::ArgAction::Count,
        help = "Increase logging verbosity"
    )]
    pub verbose: u8,

    #[arg(
        short = 'q',
        long = "quiet",
        global = true,
        default_value_t = false,
        help = "Suppress non-essential log output"
    )]
    pub quiet: bool,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ColorMode::Auto,
        help = "Color mode: auto, always, never"
    )]
    pub color: ColorMode,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Returns the canonical sandbox mode string (`"auto"`, `"full"`, `"degraded"`, or `"none"`).
    pub fn sandbox_mode_str(&self) -> &'static str {
        self.sandbox.as_str()
    }

    /// Returns effective precise mode taking `--no-precise` into account.
    pub fn effective_precise(&self) -> bool {
        self.precise && !self.no_precise
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Initialize a new problem directory structure")]
    Init {
        #[arg(default_value = ".", help = "Target problem directory")]
        dir: PathBuf,
        #[arg(
            long = "type",
            value_enum,
            default_value_t = InitProblemType::Traditional,
            help = "Problem type: traditional, interactive, grader, communication, output_only"
        )]
        problem_type: InitProblemType,
        #[arg(long = "name", help = "Problem identifier name")]
        name: Option<String>,
        #[arg(long = "title", help = "Human-readable problem title")]
        title: Option<String>,
        #[arg(
            long = "force",
            default_value_t = false,
            help = "Overwrite existing files if problem.toml already exists"
        )]
        force: bool,
    },

    #[command(about = "Compile problem binaries (checker, validator, solutions)")]
    Build {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(help = "Specific build targets or empty for all")]
        targets: Vec<String>,
    },

    #[command(about = "Generate test inputs according to [[tests.gen]]")]
    Gen {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(
            long = "force",
            default_value_t = false,
            help = "Regenerate tests even if cache matches"
        )]
        force: bool,
        #[arg(
            long = "append",
            default_value_t = false,
            help = "Append new tests after existing tests instead of starting from 1"
        )]
        append: bool,
    },

    #[command(about = "Validate test inputs with validator")]
    Validate {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Generate expected .ans files using main solution")]
    Ans {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(
            long = "force",
            default_value_t = false,
            help = "Regenerate answers even if cache matches"
        )]
        force: bool,
    },

    #[command(about = "Judge one or more submission sources against problem tests")]
    Judge {
        #[arg(required = true, help = "Path to submission source file(s)")]
        sources: Vec<PathBuf>,
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory containing problem.toml"
        )]
        problem: PathBuf,
    },

    #[command(about = "Verify all [[solutions]] against test data and check expected verdicts")]
    Verify {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Calibrate CPU time limit recommendation")]
    Tl {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(
            long = "apply",
            default_value_t = false,
            help = "Write recommended TL back to problem.toml"
        )]
        apply: bool,
    },

    #[command(about = "Differential testing (stress testing)")]
    Stress {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(long = "gen", required = true, help = "Generator executable/source")]
        generator: String,
        #[arg(long = "sol", required = true, help = "Target solution to test")]
        solution: String,
        #[arg(
            long = "brute",
            required = true,
            help = "Brute force solution as oracle"
        )]
        brute: String,
        #[arg(
            short = 'n',
            long = "count",
            default_value_t = 1000,
            help = "Maximum test iterations"
        )]
        count: usize,
        #[arg(long = "seed", help = "Initial random seed")]
        seed: Option<u64>,
        #[arg(
            long = "gen-args",
            default_value = "{seed}",
            help = "Generator argument template (supports {seed} and {iter})"
        )]
        gen_args: String,
        #[arg(
            long = "time-budget",
            default_value = "60s",
            value_parser = parse_duration_str,
            help = "Time budget (e.g. 500ms, 60s, 5m, 1h)"
        )]
        time_budget: Duration,
    },

    #[command(about = "Clean orphan test files and package test data into a flat ZIP archive")]
    Pack {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
        #[arg(
            short = 'o',
            long = "output",
            default_value = "data.zip",
            help = "Output ZIP archive path"
        )]
        output: PathBuf,
        #[arg(
            long = "clean",
            default_value_t = false,
            help = "Remove orphan/stale files in tests/ not matching current problem.toml"
        )]
        clean: bool,
    },

    #[command(about = "Perform system and problem environment health checks")]
    Doctor {
        #[arg(
            short = 'p',
            long = "problem",
            value_name = "DIR",
            num_args = 0..=1,
            default_missing_value = ".",
            help = "Run problem test data and config health checks"
        )]
        problem: Option<PathBuf>,
        #[arg(
            long = "fix-apparmor",
            default_value_t = false,
            help = "Print Ubuntu unprivileged userns AppArmor profile instructions"
        )]
        fix_apparmor: bool,
    },

    #[command(about = "Judge full contest directory (Lemon / LemonLime style)")]
    Contest {
        #[arg(required = true, help = "Contest root directory")]
        dir: PathBuf,
        #[arg(
            long = "problem",
            value_delimiter = ',',
            help = "Filter by problem name(s) (comma-separated or repeated)"
        )]
        problems: Vec<String>,
        #[arg(
            long = "contestant",
            value_delimiter = ',',
            help = "Filter by contestant name(s) (comma-separated or repeated)"
        )]
        contestants: Vec<String>,
    },

    #[command(about = "Import problem configuration from external format (e.g., Lemon)")]
    Import {
        #[arg(value_name = "FORMAT", value_parser = ["lemon"], help = "Format identifier")]
        kind: String,
        #[arg(required = true, help = "Path to CDF / project file")]
        file: PathBuf,
        #[arg(
            long = "force",
            default_value_t = false,
            help = "Overwrite existing problem.toml files"
        )]
        force: bool,
    },

    #[command(about = "Generate export report in HTML or CSV format")]
    Report {
        #[arg(help = "Run ID or empty for last run")]
        run_id: Option<String>,
        #[arg(long = "html", help = "Export HTML report file")]
        html: Option<PathBuf>,
        #[arg(long = "csv", help = "Export CSV report file")]
        csv: Option<PathBuf>,
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Show summary of the most recent judging run")]
    Last {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Show detailed inspection of a run or single test case")]
    Show {
        #[arg(help = "Run ID")]
        run_id: Option<String>,
        #[arg(long = "test", help = "Specific test ID to inspect")]
        test: Option<String>,
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Print JSON Schema for result, config, or events")]
    Schema {
        #[arg(
            value_enum,
            default_value = "result",
            help = "Schema target: result, config, events"
        )]
        target: SchemaTarget,
    },

    #[command(about = "Run command in sandbox for inspection/debugging")]
    Run {
        #[arg(
            trailing_var_arg = true,
            required = true,
            help = "Command and arguments to execute"
        )]
        cmd: Vec<String>,
    },

    #[command(about = "Clean cache and build artifacts")]
    Clean {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(
        about = "Execute full problem preparation pipeline (build -> gen -> validate -> ans -> verify -> tl)"
    )]
    Pipeline {
        #[arg(
            short = 'p',
            long = "problem",
            default_value = ".",
            help = "Problem directory"
        )]
        problem: PathBuf,
    },

    #[command(about = "Start Model Context Protocol (MCP) server over stdio")]
    Mcp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_str() {
        assert_eq!(
            parse_duration_str("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_duration_str("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(
            parse_duration_str("1.5s").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_duration_str("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration_str("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration_str("15").unwrap(), Duration::from_secs(15));

        assert!(parse_duration_str("").is_err());
        assert!(parse_duration_str("0s").is_err());
        assert!(parse_duration_str("-5s").is_err());
        assert!(parse_duration_str("abc").is_err());
        assert!(parse_duration_str("10d").is_err());
    }

    #[test]
    fn test_doctor_and_precise_flags() {
        let c1 = Cli::try_parse_from(["uj", "doctor"]).unwrap();
        match c1.command {
            Commands::Doctor { problem, .. } => assert_eq!(problem, None),
            _ => panic!("expected Doctor"),
        }

        let c2 = Cli::try_parse_from(["uj", "doctor", "--problem", "--format", "json"]).unwrap();
        match c2.command {
            Commands::Doctor { problem, .. } => assert_eq!(problem, Some(PathBuf::from("."))),
            _ => panic!("expected Doctor"),
        }

        let c3 = Cli::try_parse_from(["uj", "doctor", "-p", "my_prob"]).unwrap();
        match c3.command {
            Commands::Doctor { problem, .. } => {
                assert_eq!(problem, Some(PathBuf::from("my_prob")))
            }
            _ => panic!("expected Doctor"),
        }

        let c4 = Cli::try_parse_from(["uj", "--no-precise", "last", "-p", "prob"]).unwrap();
        assert!(!c4.effective_precise());

        let c5 = Cli::try_parse_from(["uj", "--precise", "true", "last"]).unwrap();
        assert!(c5.effective_precise());
    }
}

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Ndjson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum SandboxMode {
    #[default]
    Auto,
    Full,
    Degraded,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SchemaTarget {
    Result,
    Config,
    Events,
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

    #[arg(long, global = true, default_value_t = true, action = clap::ArgAction::Set, help = "Enable precise rejudging for edge cases")]
    pub precise: bool,

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

    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count, help = "Increase logging verbosity")]
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
        default_value = "auto",
        help = "Color mode: auto, always, never"
    )]
    pub color: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Initialize a new problem directory structure")]
    Init {
        #[arg(default_value = ".", help = "Target problem directory")]
        dir: PathBuf,
        #[arg(long = "type", default_value = "traditional", help = "Problem type")]
        problem_type: String,
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
        #[arg(short = 'n', default_value_t = 1000, help = "Maximum test iterations")]
        count: usize,
        #[arg(long = "seed", help = "Initial random seed")]
        seed: Option<u64>,
        #[arg(
            long = "time-budget",
            default_value = "60s",
            help = "Time budget (e.g. 60s)"
        )]
        time_budget: String,
    },

    #[command(about = "Perform system and problem environment health checks")]
    Doctor {
        #[arg(
            long = "problem",
            default_value_t = false,
            help = "Run problem test data and config health checks"
        )]
        problem: bool,
        #[arg(
            long = "fix-apparmor",
            default_value_t = false,
            help = "Print/install Ubuntu unprivileged userns AppArmor profile"
        )]
        fix_apparmor: bool,
    },

    #[command(about = "Judge full contest directory (Lemon / LemonLime style)")]
    Contest {
        #[arg(required = true, help = "Contest root directory")]
        dir: PathBuf,
    },

    #[command(about = "Import problem configuration from external format (e.g., Lemon)")]
    Import {
        #[arg(value_name = "FORMAT", value_parser = ["lemon"], help = "Format identifier")]
        kind: String,
        #[arg(required = true, help = "Path to CDF / project file")]
        file: PathBuf,
    },

    #[command(about = "Generate export report in HTML or CSV format")]
    Report {
        #[arg(help = "Run ID or empty for last run")]
        run_id: Option<String>,
        #[arg(long = "html", help = "Export HTML report file")]
        html: Option<PathBuf>,
        #[arg(long = "csv", help = "Export CSV report file")]
        csv: Option<PathBuf>,
    },

    #[command(about = "Show summary of the most recent judging run")]
    Last,

    #[command(about = "Show detailed inspection of a run or single test case")]
    Show {
        #[arg(help = "Run ID")]
        run_id: Option<String>,
        #[arg(long = "test", help = "Specific test ID to inspect")]
        test: Option<String>,
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
    Clean,

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
}

//! Command handler for `uj report`.

use crate::cli::OutputFormat;
use crate::error::{EXIT_JUDGE_FAILED, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use uj_report::result::SubmissionJudgeReport;

pub fn run_report(
    problem_dir: &Path,
    run_id: Option<&str>,
    html_target: Option<&Path>,
    csv_target: Option<&Path>,
    format: OutputFormat,
) -> ExitCode {
    let run_file = if let Some(id) = run_id {
        let name = if id.ends_with(".json") {
            id.to_string()
        } else {
            format!("{id}.json")
        };
        let p = problem_dir.join(".uj").join("runs").join(&name);
        if p.exists() {
            p
        } else {
            PathBuf::from(".uj").join("runs").join(&name)
        }
    } else {
        let p = problem_dir.join(".uj").join("runs").join("last.json");
        if p.exists() {
            p
        } else {
            PathBuf::from(".uj").join("runs").join("last.json")
        }
    };

    if !run_file.exists() {
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "ReportNotFound",
                    "message": format!("Run report '{}' does not exist.", run_file.display()),
                    "hint": "Run 'uj judge' first to generate a run report."
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!("Run report '{}' does not exist.", run_file.display());
            eprintln!("Hint: Run 'uj judge' first to generate a run report.");
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let content = match fs::read_to_string(&run_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read run report '{}: {e}", run_file.display());
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let report: SubmissionJudgeReport = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to parse run report '{}: {e}", run_file.display());
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    let mut generated_html: Option<PathBuf> = None;
    let mut generated_csv: Option<PathBuf> = None;

    // Default to report.html if neither --html nor --csv was specified
    let target_html = if html_target.is_none() && csv_target.is_none() {
        Some(PathBuf::from("report.html"))
    } else {
        html_target.map(|p| p.to_path_buf())
    };

    if let Some(ref out_html) = target_html {
        match uj_report::generate_html(&report) {
            Ok(html_content) => {
                if let Err(e) = fs::write(out_html, html_content) {
                    eprintln!(
                        "Failed to write HTML report to '{}': {e}",
                        out_html.display()
                    );
                    return ExitCode::from(EXIT_JUDGE_FAILED);
                }
                generated_html = Some(out_html.clone());
            }
            Err(e) => {
                eprintln!("Failed to generate HTML report: {e}");
                return ExitCode::from(EXIT_JUDGE_FAILED);
            }
        }
    }

    if let Some(out_csv) = csv_target {
        let csv_content = uj_report::generate_csv(&report);
        if let Err(e) = fs::write(out_csv, csv_content) {
            eprintln!("Failed to write CSV report to '{}': {e}", out_csv.display());
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
        generated_csv = Some(out_csv.to_path_buf());
    }

    if format == OutputFormat::Json {
        let res = serde_json::json!({
            "ok": true,
            "problem": report.problem,
            "submission": report.submission,
            "verdict": report.verdict,
            "score": report.score,
            "html": generated_html.map(|p| p.to_string_lossy().to_string()),
            "csv": generated_csv.map(|p| p.to_string_lossy().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&res).unwrap());
    } else {
        if let Some(ref p) = generated_html {
            println!("HTML report exported: {}", p.display());
        }
        if let Some(ref p) = generated_csv {
            println!("CSV report exported: {}", p.display());
        }
    }

    ExitCode::from(EXIT_SUCCESS)
}

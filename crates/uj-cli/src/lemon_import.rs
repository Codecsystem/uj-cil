//! LemonLime `.cdf` (Contest Definition File) import logic.

use crate::cli::OutputFormat;
use crate::error::{
    EXIT_JUDGE_FAILED, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG, atomic_write_bytes, emit_error,
};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use uj_core::config::ProblemConfig;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct LemonCdf {
    pub version: Option<String>,
    pub contest_title: Option<String>,
    #[serde(default)]
    pub tasks: Vec<LemonTask>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct LemonTask {
    pub problem_title: Option<String>,
    pub source_file_name: String,
    pub input_file_name: Option<String>,
    pub output_file_name: Option<String>,
    pub standard_input_check: Option<bool>,
    pub standard_output_check: Option<bool>,
    pub task_type: Option<i32>,
    pub comparison_mode: Option<i32>,
    pub real_precision: Option<i32>,
    pub special_judge: Option<String>,
    pub interactor: Option<String>,
    pub grader: Option<String>,
    #[serde(default)]
    pub test_cases: Vec<LemonTestCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct LemonTestCase {
    pub full_score: Option<f64>,
    pub time_limit: Option<u64>,
    pub memory_limit: Option<u64>,
    #[serde(default)]
    pub input_files: Vec<String>,
    #[serde(default)]
    pub output_files: Vec<String>,
}

/// Result of importing a single Lemon task.
pub struct ImportedTask {
    pub name: String,
    pub path: PathBuf,
    pub subtask_count: usize,
    pub total_score: f64,
}

/// Validate that a Lemon task's `sourceFileName` is a safe single directory component.
pub fn validate_source_file_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Lemon task 'sourceFileName' cannot be empty".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!(
            "Invalid Lemon task 'sourceFileName' '{name}': '.' and '..' are not allowed"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "Invalid Lemon task 'sourceFileName' '{name}': must match ^[A-Za-z0-9_.-]+$ with no path separators"
        ));
    }
    Ok(())
}

/// Format a string as a properly escaped TOML string literal (`"..."`).
fn toml_quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Convert Lemon comparison mode to checker settings in problem.toml
fn map_checker(mode: i32, spj: Option<&str>, real_prec: Option<i32>) -> (String, Option<String>) {
    match mode {
        0 => ("builtin".to_string(), Some("fcmp".to_string())),
        1 => ("builtin".to_string(), Some("wcmp".to_string())),
        3 => {
            let name = match real_prec.unwrap_or(6) {
                4 => "rcmp4",
                9 => "rcmp9",
                _ => "rcmp6",
            };
            ("builtin".to_string(), Some(name.to_string()))
        }
        4 => ("lemon".to_string(), spj.map(|s| s.to_string())),
        5 => ("testlib".to_string(), spj.map(|s| s.to_string())),
        _ => ("builtin".to_string(), Some("wcmp".to_string())),
    }
}

/// Converts a LemonTask to standard problem.toml content using safe TOML string serialization.
pub fn generate_problem_toml(task: &LemonTask, tests_dir: Option<&str>) -> String {
    let mut toml = String::new();

    let problem_type = match task.task_type.unwrap_or(0) {
        1 => "output_only",
        2 => "interactive",
        3 | 4 => "communication",
        _ => "traditional",
    };

    toml.push_str("schema = 1\n");
    toml.push_str(&format!("name = {}\n", toml_quote(&task.source_file_name)));
    if let Some(ref title) = task.problem_title {
        toml.push_str(&format!("title = {}\n", toml_quote(title)));
    }
    toml.push_str(&format!("type = {}\n\n", toml_quote(problem_type)));

    let time_limit = task
        .test_cases
        .first()
        .and_then(|tc| tc.time_limit)
        .unwrap_or(1000);

    let memory_limit = task
        .test_cases
        .first()
        .and_then(|tc| tc.memory_limit)
        .unwrap_or(512);

    toml.push_str("[limits]\n");
    toml.push_str(&format!("time = \"{time_limit}ms\"\n"));
    if memory_limit <= 4096 {
        toml.push_str(&format!("memory = \"{memory_limit}MB\"\n\n"));
    } else {
        toml.push_str(&format!("memory = \"{memory_limit}KB\"\n\n"));
    }

    toml.push_str("[io]\n");
    let stdin_check = task.standard_input_check.unwrap_or(true);
    let stdout_check = task.standard_output_check.unwrap_or(true);

    if !stdin_check || !stdout_check {
        toml.push_str("mode = \"file\"\n");
    }
    if !stdin_check && let Some(ref inf) = task.input_file_name {
        toml.push_str(&format!("input = {}\n", toml_quote(inf)));
    }
    if !stdout_check && let Some(ref outf) = task.output_file_name {
        toml.push_str(&format!("output = {}\n", toml_quote(outf)));
    }
    toml.push('\n');

    let (checker_type, checker_name_or_path) = map_checker(
        task.comparison_mode.unwrap_or(1),
        task.special_judge.as_deref(),
        task.real_precision,
    );

    toml.push_str("[checker]\n");
    toml.push_str(&format!("type = {}\n", toml_quote(&checker_type)));
    if checker_type == "builtin" {
        if let Some(ref name) = checker_name_or_path {
            toml.push_str(&format!("name = {}\n", toml_quote(name)));
        }
    } else if let Some(ref path) = checker_name_or_path {
        toml.push_str(&format!("path = {}\n", toml_quote(path)));
    }
    toml.push('\n');

    if let Some(td) = tests_dir {
        toml.push_str("[tests]\n");
        toml.push_str(&format!("dir = {}\n\n", toml_quote(td)));
    }

    // Subtasks
    if task.test_cases.len() > 1
        || task.test_cases.iter().any(|tc| {
            tc.input_files
                .iter()
                .any(|f| f.ends_with("_lemon_SUbtaskDEPENDENCE_fLAg"))
        })
    {
        for (idx, tc) in task.test_cases.iter().enumerate() {
            let subtask_id = idx + 1;
            let score = tc.full_score.unwrap_or(10.0);

            let mut real_inputs = Vec::new();
            let mut depends = Vec::new();

            for f in &tc.input_files {
                if let Some(dep_str) = f.strip_suffix("_lemon_SUbtaskDEPENDENCE_fLAg") {
                    if let Ok(dep_idx) = dep_str.parse::<u32>() {
                        depends.push(dep_idx);
                    }
                } else {
                    real_inputs.push(f.clone());
                }
            }

            toml.push_str("[[subtasks]]\n");
            toml.push_str(&format!("id = {subtask_id}\n"));
            toml.push_str(&format!("score = {score:.1}\n"));

            if !depends.is_empty() {
                let dep_strs: Vec<String> = depends.iter().map(|d| d.to_string()).collect();
                toml.push_str(&format!("depends = [{}]\n", dep_strs.join(", ")));
            }

            if !real_inputs.is_empty() {
                let formatted_tests: Vec<String> = real_inputs
                    .iter()
                    .map(|f| {
                        let normalized = f.replace('\\', "/");
                        if let Some(stem) =
                            Path::new(&normalized).file_stem().and_then(|s| s.to_str())
                        {
                            toml_quote(stem)
                        } else {
                            toml_quote(f)
                        }
                    })
                    .collect();
                toml.push_str(&format!("tests = [{}]\n", formatted_tests.join(", ")));
            }
            toml.push('\n');
        }
    }

    toml
}

/// Parses and imports a LemonLime `.cdf` file into the target directory.
pub fn run_import_lemon(cdf_path: &Path, force: bool, format: OutputFormat) -> ExitCode {
    if !cdf_path.exists() {
        emit_error(
            format,
            "FileNotFound",
            format!("Lemon CDF file '{}' does not exist.", cdf_path.display()),
            None,
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let raw_bytes = match fs::read(cdf_path) {
        Ok(b) => b,
        Err(e) => {
            emit_error(
                format,
                "IoError",
                format!("Failed to read CDF file '{}': {e}", cdf_path.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    // Check if binary legacy Lemon CDF
    let legacy_msg = "Legacy Lemon binary .cdf is not supported. Please open the contest in LemonLime and save it to convert to modern JSON .cdf format.";
    let content_str = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            emit_error(
                format,
                "LegacyBinaryCdfNotSupported",
                legacy_msg,
                Some("Use LemonLime to open and re-save the contest as JSON .cdf"),
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    if !content_str.trim_start().starts_with('{') {
        emit_error(
            format,
            "LegacyBinaryCdfNotSupported",
            legacy_msg,
            Some("Use LemonLime to open and re-save the contest as JSON .cdf"),
        );
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let cdf: LemonCdf = match serde_json::from_str(&content_str) {
        Ok(parsed) => parsed,
        Err(e) => {
            emit_error(
                format,
                "InvalidJsonCdf",
                format!("Failed to parse LemonLime JSON .cdf: {e}"),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let base_dir = cdf_path.parent().unwrap_or_else(|| Path::new("."));

    // Phase 1: Validate all task names, existing files, and generated TOML in memory
    // before creating any directories or touching the filesystem. Never call remove_dir_all.
    struct PreparedImport<'a> {
        task: &'a LemonTask,
        problem_dir: PathBuf,
        toml_path: PathBuf,
        toml_str: String,
    }

    let mut prepared = Vec::with_capacity(cdf.tasks.len());
    for task in &cdf.tasks {
        if let Err(msg) = validate_source_file_name(&task.source_file_name) {
            emit_error(
                format,
                "InvalidSourceFileName",
                msg,
                Some(
                    "Each task's sourceFileName must be a safe single directory name matching ^[A-Za-z0-9_.-]+$",
                ),
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        let problem_dir = base_dir.join("problems").join(&task.source_file_name);
        let toml_path = problem_dir.join("problem.toml");

        if !force && toml_path.exists() {
            emit_error(
                format,
                "AlreadyExists",
                format!(
                    "Refusing to overwrite existing '{}' (pass --force to overwrite)",
                    toml_path.display()
                ),
                Some(
                    "Run with `uj import lemon <file> --force` to overwrite existing problem.toml files",
                ),
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        let data_dir_candidate = base_dir.join("data").join(&task.source_file_name);
        let tests_dir_override = if data_dir_candidate.exists() {
            Some(format!("../../data/{}", task.source_file_name))
        } else {
            None
        };

        let toml_str = generate_problem_toml(task, tests_dir_override.as_deref());

        if let Err(e) = ProblemConfig::from_toml_str(&toml_str, None) {
            emit_error(
                format,
                "InvalidImportedConfig",
                format!(
                    "Imported problem.toml for '{}' failed validation: {e}",
                    task.source_file_name
                ),
                None,
            );
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        prepared.push(PreparedImport {
            task,
            problem_dir,
            toml_path,
            toml_str,
        });
    }

    // Phase 2: Write validated configs to disk
    let mut imported = Vec::with_capacity(prepared.len());
    for item in prepared {
        if let Err(e) = fs::create_dir_all(&item.problem_dir) {
            emit_error(
                format,
                "IoError",
                format!(
                    "Failed to create directory '{}': {e}",
                    item.problem_dir.display()
                ),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        if let Err(e) = atomic_write_bytes(&item.toml_path, item.toml_str.as_bytes()) {
            emit_error(
                format,
                "IoError",
                format!("Failed to write '{}': {e}", item.toml_path.display()),
                None,
            );
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        let total_score: f64 = item
            .task
            .test_cases
            .iter()
            .map(|tc| tc.full_score.unwrap_or(0.0))
            .sum();

        imported.push(ImportedTask {
            name: item.task.source_file_name.clone(),
            path: item.toml_path,
            subtask_count: item.task.test_cases.len(),
            total_score,
        });
    }

    let task_items: Vec<serde_json::Value> = imported
        .iter()
        .map(|it| {
            serde_json::json!({
                "name": it.name,
                "problem_toml": it.path.to_string_lossy().to_string(),
                "subtasks": it.subtask_count,
                "total_score": it.total_score,
            })
        })
        .collect();

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "contest_title": cdf.contest_title.unwrap_or_default(),
                "imported_tasks": task_items,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        OutputFormat::Ndjson => {
            let out = serde_json::json!({
                "schema_version": 1,
                "ok": true,
                "contest_title": cdf.contest_title.unwrap_or_default(),
                "imported_tasks": task_items,
            });
            println!("{}", serde_json::to_string(&out).unwrap());
        }
        OutputFormat::Human => {
            println!(
                "Successfully imported contest: {}",
                cdf.contest_title.as_deref().unwrap_or("Untitled Contest")
            );
            for it in &imported {
                println!(
                    "  - Task '{}': created {} ({} subtasks, {:.0} pts)",
                    it.name,
                    it.path.display(),
                    it.subtask_count,
                    it.total_score
                );
            }
        }
        OutputFormat::Silent => {}
    }

    ExitCode::from(EXIT_SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_problem_toml_from_lemon_task() {
        let task = LemonTask {
            problem_title: Some("A Plus B".to_string()),
            source_file_name: "aplusb".to_string(),
            input_file_name: Some("aplusb.in".to_string()),
            output_file_name: Some("aplusb.out".to_string()),
            standard_input_check: Some(true),
            standard_output_check: Some(true),
            task_type: Some(0),
            comparison_mode: Some(1),
            real_precision: None,
            special_judge: None,
            interactor: None,
            grader: None,
            test_cases: vec![
                LemonTestCase {
                    full_score: Some(40.0),
                    time_limit: Some(1000),
                    memory_limit: Some(256),
                    input_files: vec!["1.in".to_string(), "2.in".to_string()],
                    output_files: vec!["1.out".to_string(), "2.out".to_string()],
                },
                LemonTestCase {
                    full_score: Some(60.0),
                    time_limit: Some(1000),
                    memory_limit: Some(256),
                    input_files: vec![
                        "3.in".to_string(),
                        "1_lemon_SUbtaskDEPENDENCE_fLAg".to_string(),
                    ],
                    output_files: vec!["3.out".to_string()],
                },
            ],
        };

        let toml_str = generate_problem_toml(&task, Some("tests"));
        assert!(toml_str.contains("name = \"aplusb\""));
        assert!(toml_str.contains("title = \"A Plus B\""));
        assert!(toml_str.contains("time = \"1000ms\""));
        assert!(toml_str.contains("memory = \"256MB\""));
        assert!(toml_str.contains("type = \"builtin\""));
        assert!(toml_str.contains("name = \"wcmp\""));
        assert!(toml_str.contains("dir = \"tests\""));
        assert!(toml_str.contains("id = 1"));
        assert!(toml_str.contains("score = 40.0"));
        assert!(toml_str.contains("id = 2"));
        assert!(toml_str.contains("score = 60.0"));
        assert!(toml_str.contains("depends = [1]"));
    }

    #[test]
    fn test_lemon_import_escapes_backslashes_and_quotes_and_rejects_traversal() {
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", "foo bar"] {
            assert!(
                validate_source_file_name(bad).is_err(),
                "Expected '{bad}' to be rejected"
            );
        }

        let task = LemonTask {
            problem_title: Some("Title with \"quotes\"\nand \\backslash".to_string()),
            source_file_name: "safe_prob".to_string(),
            input_file_name: Some("sub\\in.txt".to_string()),
            output_file_name: Some("sub\\out.txt".to_string()),
            standard_input_check: Some(false),
            standard_output_check: Some(false),
            task_type: Some(0),
            comparison_mode: Some(4),
            real_precision: None,
            special_judge: Some("data\\chk.exe".to_string()),
            interactor: None,
            grader: None,
            test_cases: vec![LemonTestCase {
                full_score: Some(100.0),
                time_limit: Some(1000),
                memory_limit: Some(256),
                input_files: vec!["sub\\1.in".to_string()],
                output_files: vec!["sub\\1.out".to_string()],
            }],
        };

        let toml_str = generate_problem_toml(&task, Some("..\\..\\data\\safe_prob"));
        let cfg = ProblemConfig::from_toml_str(&toml_str, None)
            .expect("Windows backslashes and quotes must serialize to valid TOML");
        assert_eq!(cfg.name, "safe_prob");
        let parsed_val: toml::Value = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed_val["title"].as_str(),
            Some("Title with \"quotes\"\nand \\backslash")
        );
    }
}

//! LemonLime `.cdf` (Contest Definition File) import logic.

use crate::cli::OutputFormat;
use crate::error::{EXIT_JUDGE_FAILED, EXIT_SUCCESS, EXIT_USAGE_OR_CONFIG};
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

/// Converts a LemonTask to standard problem.toml content.
pub fn generate_problem_toml(task: &LemonTask, tests_dir: Option<&str>) -> String {
    let mut toml = String::new();

    let problem_type = match task.task_type.unwrap_or(0) {
        1 => "output_only",
        2 => "interactive",
        3 | 4 => "communication",
        _ => "traditional",
    };

    toml.push_str("schema = 1\n");
    toml.push_str(&format!("name = \"{}\"\n", task.source_file_name));
    if let Some(ref title) = task.problem_title {
        toml.push_str(&format!("title = \"{title}\"\n"));
    }
    toml.push_str(&format!("type = \"{problem_type}\"\n\n"));

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
        toml.push_str(&format!("input = \"{inf}\"\n"));
    }
    if !stdout_check && let Some(ref outf) = task.output_file_name {
        toml.push_str(&format!("output = \"{outf}\"\n"));
    }
    toml.push('\n');

    let (checker_type, checker_name_or_path) = map_checker(
        task.comparison_mode.unwrap_or(1),
        task.special_judge.as_deref(),
        task.real_precision,
    );

    toml.push_str("[checker]\n");
    toml.push_str(&format!("type = \"{checker_type}\"\n"));
    if checker_type == "builtin" {
        if let Some(ref name) = checker_name_or_path {
            toml.push_str(&format!("name = \"{name}\"\n"));
        }
    } else if let Some(ref path) = checker_name_or_path {
        toml.push_str(&format!("path = \"{path}\"\n"));
    }
    toml.push('\n');

    if let Some(td) = tests_dir {
        toml.push_str("[tests]\n");
        toml.push_str(&format!("dir = \"{td}\"\n\n"));
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
                        // Strip extension for problem.toml test pattern
                        if let Some(stem) = Path::new(f).file_stem().and_then(|s| s.to_str()) {
                            format!("\"{stem}\"")
                        } else {
                            format!("\"{f}\"")
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
pub fn run_import_lemon(cdf_path: &Path, format: OutputFormat) -> ExitCode {
    if !cdf_path.exists() {
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "FileNotFound",
                    "message": format!("Lemon CDF file '{}' does not exist.", cdf_path.display())
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!("Lemon CDF file '{}' does not exist.", cdf_path.display());
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let raw_bytes = match fs::read(cdf_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to read CDF file: {e}");
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }
    };

    // Check if binary legacy Lemon CDF
    let content_str = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            let msg = "Legacy Lemon binary .cdf is not supported. Please open the contest in LemonLime and save it to convert to modern JSON .cdf format.";
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "LegacyBinaryCdfNotSupported",
                        "message": msg,
                        "hint": "Use LemonLime to open and re-save the contest as JSON .cdf"
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!("Error: {msg}");
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    if !content_str.trim_start().starts_with('{') {
        let msg = "Legacy Lemon binary .cdf is not supported. Please open the contest in LemonLime and save it to convert to modern JSON .cdf format.";
        if format == OutputFormat::Json {
            let err_obj = serde_json::json!({
                "ok": false,
                "error": {
                    "kind": "LegacyBinaryCdfNotSupported",
                    "message": msg,
                    "hint": "Use LemonLime to open and re-save the contest as JSON .cdf"
                }
            });
            println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
        } else {
            eprintln!("Error: {msg}");
        }
        return ExitCode::from(EXIT_USAGE_OR_CONFIG);
    }

    let cdf: LemonCdf = match serde_json::from_str(&content_str) {
        Ok(parsed) => parsed,
        Err(e) => {
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "InvalidJsonCdf",
                        "message": format!("Failed to parse LemonLime JSON .cdf: {e}")
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!("Failed to parse LemonLime JSON .cdf: {e}");
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }
    };

    let base_dir = cdf_path.parent().unwrap_or_else(|| Path::new("."));
    let mut imported = Vec::new();

    for task in &cdf.tasks {
        // Target directory: problems/<source_file_name> or base_dir/problems/<source_file_name>
        let problem_dir = base_dir.join("problems").join(&task.source_file_name);
        let _ = fs::create_dir_all(&problem_dir);

        // Check if data directory exists in base_dir/data/<source_file_name>
        let data_dir_candidate = base_dir.join("data").join(&task.source_file_name);
        let tests_dir_override = if data_dir_candidate.exists() {
            Some(format!("../../data/{}", task.source_file_name))
        } else {
            None
        };

        let toml_str = generate_problem_toml(task, tests_dir_override.as_deref());
        let toml_path = problem_dir.join("problem.toml");

        if let Err(e) = fs::write(&toml_path, &toml_str) {
            eprintln!("Failed to write {}: {e}", toml_path.display());
            return ExitCode::from(EXIT_JUDGE_FAILED);
        }

        if let Err(e) = ProblemConfig::from_toml_str(&toml_str, Some(&toml_path)) {
            let _ = fs::remove_dir_all(&problem_dir);
            if format == OutputFormat::Json {
                let err_obj = serde_json::json!({
                    "ok": false,
                    "error": {
                        "kind": "InvalidImportedConfig",
                        "message": format!("Imported problem.toml for '{}' failed validation: {e}", task.source_file_name)
                    }
                });
                println!("{}", serde_json::to_string_pretty(&err_obj).unwrap());
            } else {
                eprintln!(
                    "Imported problem.toml for '{}' failed validation: {e}",
                    task.source_file_name
                );
            }
            return ExitCode::from(EXIT_USAGE_OR_CONFIG);
        }

        let total_score: f64 = task
            .test_cases
            .iter()
            .map(|tc| tc.full_score.unwrap_or(0.0))
            .sum();

        imported.push(ImportedTask {
            name: task.source_file_name.clone(),
            path: toml_path,
            subtask_count: task.test_cases.len(),
            total_score,
        });
    }

    if format == OutputFormat::Json {
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

        let out = serde_json::json!({
            "ok": true,
            "contest_title": cdf.contest_title.unwrap_or_default(),
            "imported_tasks": task_items,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else if format == OutputFormat::Human {
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
}

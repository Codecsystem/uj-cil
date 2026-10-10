use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use tempfile::tempdir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn uj_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("uj.exe");
    if !path.exists() {
        path.set_extension("");
    }
    path
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[test]
fn test_html_and_csv_report_generation() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let uj = uj_bin();
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path();

    // Copy aplusb problem fixture to temp_dir
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/problems/aplusb");
    copy_dir_all(&fixture_dir, root).unwrap();

    let sol_path = root.join("solutions").join("ac.cpp");

    // 1. Judge solution to produce .uj/runs/last.json
    let judge_output = Command::new(&uj)
        .arg("judge")
        .arg(&sol_path)
        .arg("-p")
        .arg(root)
        .current_dir(root)
        .output()
        .expect("failed to execute uj judge");

    assert!(
        judge_output.status.success(),
        "Judge should pass: {}",
        String::from_utf8_lossy(&judge_output.stderr)
    );

    // 2. Generate HTML report
    let html_out = root.join("custom_report.html");
    let report_html_res = Command::new(&uj)
        .arg("report")
        .arg("--html")
        .arg(&html_out)
        .current_dir(root)
        .output()
        .expect("failed to execute uj report --html");

    assert!(
        report_html_res.status.success(),
        "uj report --html failed: {}",
        String::from_utf8_lossy(&report_html_res.stderr)
    );
    assert!(html_out.exists(), "HTML report file must be created");

    let html_content = fs::read_to_string(&html_out).unwrap();
    assert!(html_content.contains("<!DOCTYPE html>"));
    assert!(html_content.contains("aplusb"));
    assert!(html_content.contains("ac.cpp"));
    assert!(html_content.contains("badge-ac"));
    // Ensure offline self-contained (no external http links for scripts/styles)
    assert!(!html_content.contains("src=\"http"));
    assert!(!html_content.contains("href=\"http"));

    // 3. Generate CSV report
    let csv_out = root.join("custom_report.csv");
    let report_csv_res = Command::new(&uj)
        .arg("report")
        .arg("--csv")
        .arg(&csv_out)
        .current_dir(root)
        .output()
        .expect("failed to execute uj report --csv");

    assert!(
        report_csv_res.status.success(),
        "uj report --csv failed: {}",
        String::from_utf8_lossy(&report_csv_res.stderr)
    );
    assert!(csv_out.exists(), "CSV report file must be created");

    let csv_content = fs::read_to_string(&csv_out).unwrap();
    let csv_lines: Vec<&str> = csv_content.lines().collect();
    assert!(!csv_lines.is_empty());
    assert_eq!(
        csv_lines[0],
        "submission,problem,test,verdict,time_ms,memory_kb,score"
    );
    assert!(
        csv_lines
            .iter()
            .any(|line| line.contains("ac.cpp,aplusb") && line.contains(",AC,"))
    );

    // 4. Test JSON format output
    let json_report_res = Command::new(&uj)
        .arg("--format")
        .arg("json")
        .arg("report")
        .arg("--html")
        .arg(root.join("report_json.html"))
        .current_dir(root)
        .output()
        .expect("failed to execute uj report --format json");

    assert!(json_report_res.status.success());
    let stdout_str = String::from_utf8_lossy(&json_report_res.stdout);
    let json_val: serde_json::Value =
        serde_json::from_str(&stdout_str).expect("stdout must be valid JSON");
    assert_eq!(json_val["ok"], true);
    assert_eq!(json_val["verdict"], "AC");
    assert_eq!(json_val["problem"], "aplusb");

    // 5. Test default report.html generation when no flags are given
    let default_report_res = Command::new(&uj)
        .arg("report")
        .current_dir(root)
        .output()
        .expect("failed to execute uj report");
    assert!(default_report_res.status.success());
    assert!(root.join("report.html").exists());

    // 6. Test WA solution and diff block rendering
    let wa_path = root.join("solutions").join("wa.cpp");
    let _ = Command::new(&uj)
        .arg("judge")
        .arg(&wa_path)
        .arg("-p")
        .arg(root)
        .current_dir(root)
        .output()
        .expect("failed to judge wa");

    let wa_html_out = root.join("wa_report.html");
    let wa_report_res = Command::new(&uj)
        .arg("report")
        .arg("--html")
        .arg(&wa_html_out)
        .current_dir(root)
        .output()
        .expect("failed to execute uj report for wa");
    assert!(wa_report_res.status.success());

    let wa_html = fs::read_to_string(&wa_html_out).unwrap();
    assert!(wa_html.contains("badge-wa"));
    assert!(wa_html.contains("diff-block"));
    assert!(wa_html.contains("diff-col exp"));
    assert!(wa_html.contains("diff-col got"));

    // 7. Non-existent report error handling
    let bad_report_res = Command::new(&uj)
        .arg("report")
        .arg("nonexistent_run_9999")
        .current_dir(root)
        .output()
        .expect("failed to execute uj report");
    assert!(!bad_report_res.status.success());
}

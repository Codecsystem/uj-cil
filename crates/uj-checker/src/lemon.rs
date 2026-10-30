//! Lemon SPJ protocol adapter.
//!
//! Calling convention:
//! `spj <input> <output> <answer> <full_score> <score_file> <message_file>`

use std::fs;
use std::path::Path;

use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_sandbox::{CancelToken, Sandbox};

use crate::{CheckResult, MAX_CHECKER_MESSAGE_CHARS, clip_str, reserve_scratch_file};

/// Formats `full_score` for the Lemon SPJ command-line argument (`argv[4]`).
///
/// Integral scores are formatted without a decimal point (e.g. `100` for `100.0`) so legacy SPJs
/// using `atoi` or `scanf("%d")` work unmodified, while fractional scores keep their exact decimal
/// representation (e.g. `12.5`).
pub fn format_lemon_full_score(full_score: f64) -> String {
    if full_score.is_finite() && (full_score - full_score.round()).abs() < 1e-9 {
        format!("{:.0}", full_score.round())
    } else {
        format!("{full_score}")
    }
}

/// Parse Lemon score and message files.
pub fn parse_lemon_output(
    score_content: &str,
    message_content: &str,
    full_score: f64,
) -> CheckResult {
    let parsed = score_content.trim().parse::<f64>();
    let raw_score = match parsed {
        Ok(v) if v.is_finite() => v,
        _ => {
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some("Invalid non-finite score returned by Lemon SPJ".to_string()),
                diff: None,
            };
        }
    };

    if !full_score.is_finite() || full_score <= 0.0 {
        return CheckResult {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
            message: Some("Invalid non-finite full_score for Lemon SPJ".to_string()),
            diff: None,
        };
    }

    let ratio = (raw_score / full_score).clamp(0.0, 1.0);

    let message = message_content.trim();
    let message_opt = if message.is_empty() {
        None
    } else {
        Some(clip_str(message, MAX_CHECKER_MESSAGE_CHARS).0)
    };

    let verdict = if (raw_score - full_score).abs() < 1e-6 || ratio >= 1.0 {
        Verdict::Accepted
    } else if raw_score <= 0.0 {
        Verdict::WrongAnswer
    } else {
        Verdict::PartiallyCorrect
    };

    CheckResult {
        verdict,
        score_ratio: ratio,
        message: message_opt,
        diff: None,
    }
}

/// Execute Lemon SPJ checker inside sandbox.
pub fn run_lemon_checker(
    sandbox: &dyn Sandbox,
    checker_exe: &Path,
    input_path: &Path,
    output_path: &Path,
    answer_path: &Path,
    full_score: f64,
    cancel: &CancelToken,
) -> CheckResult {
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let score_file = match reserve_scratch_file(&scratch_dir, "lemon_score", "txt") {
        Ok(p) => p,
        Err(e) => {
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some(format!("Failed to reserve Lemon score file: {e}")),
                diff: None,
            };
        }
    };
    let message_file = match reserve_scratch_file(&scratch_dir, "lemon_msg", "txt") {
        Ok(p) => p,
        Err(e) => {
            let _ = fs::remove_file(&score_file);
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some(format!("Failed to reserve Lemon message file: {e}")),
                diff: None,
            };
        }
    };

    let full_score_str = format_lemon_full_score(full_score);

    let argv = vec![
        checker_exe.as_os_str().to_os_string(),
        input_path.as_os_str().to_os_string(),
        output_path.as_os_str().to_os_string(),
        answer_path.as_os_str().to_os_string(),
        std::ffi::OsString::from(full_score_str),
        score_file.as_os_str().to_os_string(),
        message_file.as_os_str().to_os_string(),
    ];

    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir: scratch_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Pipe,
        extra_fds: vec![],
        limits: Limits::new(10000, 1024 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let run_res = match sandbox.run(&spec, cancel) {
        Ok(r) => r,
        Err(e) => {
            let _ = fs::remove_file(&score_file);
            let _ = fs::remove_file(&message_file);
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some(format!("Failed to execute Lemon SPJ: {e}")),
                diff: None,
            };
        }
    };

    let score_content = fs::read(&score_file)
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    let message_content = fs::read(&message_file)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| String::from_utf8_lossy(&run_res.stderr_tail).to_string());

    let _ = fs::remove_file(&score_file);
    let _ = fs::remove_file(&message_file);

    if !run_res.status.is_success() && score_content.is_empty() {
        return CheckResult {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
            message: Some(format!("Lemon SPJ exited with error: {:?}", run_res.status)),
            diff: None,
        };
    }

    parse_lemon_output(&score_content, &message_content, full_score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lemon_parsing() {
        let ac = parse_lemon_output("100\n", "ok, perfect match", 100.0);
        assert_eq!(ac.verdict, Verdict::Accepted);
        assert_eq!(ac.score_ratio, 1.0);
        assert_eq!(ac.message.as_deref(), Some("ok, perfect match"));

        let wa = parse_lemon_output("0\n", "wrong", 100.0);
        assert_eq!(wa.verdict, Verdict::WrongAnswer);
        assert_eq!(wa.score_ratio, 0.0);

        let pc = parse_lemon_output("60\n", "partial 60/100", 100.0);
        assert_eq!(pc.verdict, Verdict::PartiallyCorrect);
        assert!((pc.score_ratio - 0.6).abs() < 1e-6);

        let nan_res = parse_lemon_output("NaN\n", "nan score", 100.0);
        assert_eq!(nan_res.verdict, Verdict::JudgeFailed);
        assert_eq!(nan_res.score_ratio, 0.0);

        let inf_res = parse_lemon_output("inf\n", "inf score", 100.0);
        assert_eq!(inf_res.verdict, Verdict::JudgeFailed);
        assert_eq!(inf_res.score_ratio, 0.0);

        assert_eq!(format_lemon_full_score(100.0), "100");
        assert_eq!(format_lemon_full_score(12.5), "12.5");
        let frac_ac = parse_lemon_output(&format_lemon_full_score(12.5), "ok", 12.5);
        assert_eq!(frac_ac.verdict, Verdict::Accepted);
        assert_eq!(frac_ac.score_ratio, 1.0);
    }
}

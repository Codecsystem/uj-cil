//! testlib checker protocol adapter (appes XML format and exit code mappings).

use std::fs;
use std::path::Path;

use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_sandbox::{CancelToken, Sandbox};

use crate::CheckResult;

/// Parse XML content produced by testlib with `-appes` flag.
///
/// Handles formats:
/// `<result outcome="accepted">ok</result>`
/// `<result outcome="points" points="0.75">half points</result>`
/// `<result outcome="partially-correct" pctype="50">points</result>`
pub fn parse_appes_xml(xml: &str, pe_as_wa: bool) -> Option<CheckResult> {
    let lower = xml.to_lowercase();
    let start_tag = lower.find("<result")?;
    let end_tag_close = xml[start_tag..].find('>')? + start_tag;
    let tag_header = &xml[start_tag..=end_tag_close];

    // Extract attributes
    let outcome = extract_attr(tag_header, "outcome")?;
    let points_str = extract_attr(tag_header, "points");
    let pctype_str = extract_attr(tag_header, "pctype");

    // Extract message body between <result...> and </result>
    let body_start = end_tag_close + 1;
    let body_end = lower[body_start..]
        .find("</result>")
        .map(|idx| body_start + idx)
        .unwrap_or(xml.len());
    let raw_msg = xml[body_start..body_end].trim();
    let message = if raw_msg.is_empty() {
        None
    } else {
        Some(unescape_xml(raw_msg))
    };

    let outcome_norm = outcome.trim().to_lowercase();
    match outcome_norm.as_str() {
        "accepted" | "ok" => Some(CheckResult {
            verdict: Verdict::Accepted,
            score_ratio: 1.0,
            message,
            diff: None,
        }),
        "wrong-answer" | "wrong answer" | "wa" | "dirt" | "unexpected-eof" => Some(CheckResult {
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message,
            diff: None,
        }),
        "presentation-error" | "presentation error" | "pe" => {
            let verdict = if pe_as_wa {
                Verdict::WrongAnswer
            } else {
                Verdict::PresentationError
            };
            Some(CheckResult {
                verdict,
                score_ratio: 0.0,
                message,
                diff: None,
            })
        }
        "fail" | "crashed" => Some(CheckResult {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
            message,
            diff: None,
        }),
        "points" | "relative-scoring" | "partially-correct" | "partially" => {
            let ratio = if let Some(ref p_str) = points_str {
                p_str.parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0)
            } else if let Some(ref pc_str) = pctype_str {
                // pctype is integer out of 100 or exitCode
                let p = pc_str.parse::<f64>().unwrap_or(0.0);
                (p / 100.0).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let verdict = if (ratio - 1.0).abs() < 1e-6 {
                Verdict::Accepted
            } else if ratio <= 0.0 {
                Verdict::WrongAnswer
            } else {
                Verdict::PartiallyCorrect
            };

            Some(CheckResult {
                verdict,
                score_ratio: ratio,
                message,
                diff: None,
            })
        }
        _ => None,
    }
}

/// Fallback verdict mapping from testlib process exit codes when `-appes` XML is unavailable.
pub fn parse_testlib_exit_code(code: i32, stderr: &str, pe_as_wa: bool) -> CheckResult {
    let msg = stderr.trim();
    let message = if msg.is_empty() {
        None
    } else {
        Some(msg.to_string())
    };

    match code {
        0 => CheckResult {
            verdict: Verdict::Accepted,
            score_ratio: 1.0,
            message,
            diff: None,
        },
        1 => CheckResult {
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message,
            diff: None,
        },
        2 => CheckResult {
            verdict: if pe_as_wa {
                Verdict::WrongAnswer
            } else {
                Verdict::PresentationError
            },
            score_ratio: 0.0,
            message,
            diff: None,
        },
        3 => CheckResult {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
            message,
            diff: None,
        },
        4 => CheckResult {
            // DIRT in testlib is extra output after solution ended, treated as WA
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message: message.or_else(|| Some("Extra information in output (dirt)".to_string())),
            diff: None,
        },
        7 => {
            // Points exit code, try to parse score from stderr if available
            CheckResult {
                verdict: Verdict::PartiallyCorrect,
                score_ratio: 0.5,
                message,
                diff: None,
            }
        }
        8 => CheckResult {
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message: message.or_else(|| Some("Unexpected end of file".to_string())),
            diff: None,
        },
        16..=255 => {
            // Partially correct with offset 16
            let raw_score = (code - 16) as f64;
            let ratio = (raw_score / 100.0).clamp(0.0, 1.0);
            let verdict = if (ratio - 1.0).abs() < 1e-6 {
                Verdict::Accepted
            } else if ratio <= 0.0 {
                Verdict::WrongAnswer
            } else {
                Verdict::PartiallyCorrect
            };

            CheckResult {
                verdict,
                score_ratio: ratio,
                message,
                diff: None,
            }
        }
        _ => CheckResult {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
            message: Some(format!("Checker failed with unexpected exit code {code}")),
            diff: None,
        },
    }
}

/// Execute external testlib checker inside sandbox.
///
/// Running format according to testlib standard:
/// `checker <input> <output> <answer> <result_file> -appes`
pub fn run_testlib_checker(
    sandbox: &dyn Sandbox,
    checker_exe: &Path,
    input_path: &Path,
    output_path: &Path,
    answer_path: &Path,
    pe_as_wa: bool,
    cancel: &CancelToken,
) -> CheckResult {
    let scratch_dir = uj_sandbox::get_sandbox_scratch_dir();
    let result_file = scratch_dir.join(format!(
        "testlib_res_{}_{}.xml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let argv = vec![
        checker_exe.as_os_str().to_os_string(),
        input_path.as_os_str().to_os_string(),
        output_path.as_os_str().to_os_string(),
        answer_path.as_os_str().to_os_string(),
        result_file.as_os_str().to_os_string(),
        std::ffi::OsString::from("-appes"),
    ];

    // testlib checker runs with generous limits (TL = 10s, ML = 1GiB)
    let spec = RunSpec {
        argv,
        env: vec![],
        work_dir: scratch_dir,
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Pipe,
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
            let _ = fs::remove_file(&result_file);
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some(format!("Failed to execute testlib checker: {e}")),
                diff: None,
            };
        }
    };

    let stderr_str = String::from_utf8_lossy(&run_res.stderr_tail).to_string();

    // 1. Try to read and parse -appes XML result file
    let check_result = if result_file.is_file() {
        if let Ok(xml_content) = fs::read_to_string(&result_file) {
            parse_appes_xml(&xml_content, pe_as_wa)
        } else {
            None
        }
    } else {
        None
    };

    let _ = fs::remove_file(&result_file);

    if let Some(res) = check_result {
        return res;
    }

    // 2. Fall back to exit code mapping
    let exit_code = match run_res.status {
        uj_core::run::ExitStatus::Exited(c) => c,
        _ => 3, // FAIL
    };

    parse_testlib_exit_code(exit_code, &stderr_str, pe_as_wa)
}

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    // Look for pattern: attr_name\s*=\s*"value"
    let mut search_from = 0;
    while let Some(idx) = tag[search_from..].find(attr_name) {
        let abs_idx = search_from + idx;
        let rest = &tag[abs_idx + attr_name.len()..];
        let trimmed_rest = rest.trim_start();
        if let Some(after_eq) = trimmed_rest.strip_prefix('=') {
            let after_quote = after_eq.trim_start();
            if let Some(stripped) = after_quote.strip_prefix('"')
                && let Some(val_end) = stripped.find('"')
            {
                return Some(stripped[..val_end].to_string());
            } else if let Some(stripped) = after_quote.strip_prefix('\'')
                && let Some(val_end) = stripped.find('\'')
            {
                return Some(stripped[..val_end].to_string());
            }
        }
        search_from = abs_idx + attr_name.len();
    }
    None
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_appes_ok() {
        let xml = "<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n<result outcome = \"accepted\">ok 42 numbers</result>\n";
        let res = parse_appes_xml(xml, true).unwrap();
        assert_eq!(res.verdict, Verdict::Accepted);
        assert_eq!(res.score_ratio, 1.0);
        assert_eq!(res.message.as_deref(), Some("ok 42 numbers"));
    }

    #[test]
    fn test_parse_appes_wa() {
        let xml = "<result outcome=\"wrong-answer\">1st numbers differ - expected: '5', found: '6'</result>";
        let res = parse_appes_xml(xml, true).unwrap();
        assert_eq!(res.verdict, Verdict::WrongAnswer);
        assert_eq!(res.score_ratio, 0.0);
        assert!(res.message.unwrap().contains("1st numbers differ"));
    }

    #[test]
    fn test_parse_appes_pe() {
        let xml = "<result outcome=\"presentation-error\">extra tokens</result>";
        let res_pe = parse_appes_xml(xml, false).unwrap();
        assert_eq!(res_pe.verdict, Verdict::PresentationError);

        let res_wa = parse_appes_xml(xml, true).unwrap();
        assert_eq!(res_wa.verdict, Verdict::WrongAnswer);
    }

    #[test]
    fn test_parse_appes_points() {
        let xml =
            "<result outcome=\"points\" points=\"0.7500000000\">partially correct: 75%</result>";
        let res = parse_appes_xml(xml, true).unwrap();
        assert_eq!(res.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res.score_ratio, 0.75);
    }

    #[test]
    fn test_parse_appes_pctype() {
        let xml = "<result outcome = \"partially-correct\" pctype = \"40\">40 points</result>";
        let res = parse_appes_xml(xml, true).unwrap();
        assert_eq!(res.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res.score_ratio, 0.40);
    }

    #[test]
    fn test_parse_exit_codes() {
        assert_eq!(
            parse_testlib_exit_code(0, "ok", true).verdict,
            Verdict::Accepted
        );
        assert_eq!(
            parse_testlib_exit_code(1, "wa", true).verdict,
            Verdict::WrongAnswer
        );
        assert_eq!(
            parse_testlib_exit_code(2, "pe", true).verdict,
            Verdict::WrongAnswer
        );
        assert_eq!(
            parse_testlib_exit_code(2, "pe", false).verdict,
            Verdict::PresentationError
        );
        assert_eq!(
            parse_testlib_exit_code(3, "fail", true).verdict,
            Verdict::JudgeFailed
        );
        assert_eq!(
            parse_testlib_exit_code(4, "dirt", true).verdict,
            Verdict::WrongAnswer
        );
        assert_eq!(
            parse_testlib_exit_code(8, "eof", true).verdict,
            Verdict::WrongAnswer
        );

        let pc_res = parse_testlib_exit_code(66, "66 pts", true);
        assert_eq!(pc_res.verdict, Verdict::PartiallyCorrect);
        assert_eq!(pc_res.score_ratio, 0.50); // (66 - 16) / 100 = 0.50
    }
}

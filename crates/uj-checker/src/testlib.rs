//! testlib checker protocol adapter (appes XML format and exit code mappings).

use std::fs;
use std::path::Path;

use uj_core::run::{Limits, RunSpec, Stdio};
use uj_core::verdict::Verdict;
use uj_sandbox::{CancelToken, Sandbox};

use crate::{CheckResult, MAX_CHECKER_MESSAGE_CHARS, clip_str, reserve_scratch_file};

/// Normalizes a raw `testlib` points value (from `quitp(...)` in either `-appes` XML or exit-code-7
/// `stderr`) into a score ratio in `[0.0, 1.0]`.
///
/// Convention:
/// - Values in `[0.0, 1.0]` are treated directly as a fractional score ratio.
/// - Values in `(1.0, 100.0]` are treated as percentages (`points / 100.0`).
/// - Negative values, values `> 100.0`, or non-finite values (`NaN`, `Inf`) are rejected (`None`),
///   which the caller maps to [`Verdict::JudgeFailed`].
pub fn normalize_testlib_points(val: f64) -> Option<f64> {
    if !val.is_finite() || !(0.0..=100.0).contains(&val) {
        return None;
    }
    if val <= 1.0 {
        Some(val)
    } else {
        Some((val / 100.0).clamp(0.0, 1.0))
    }
}

/// Parse XML content produced by testlib with `-appes` flag.
///
/// Handles formats:
/// `<result outcome="accepted">ok</result>`
/// `<result outcome="points" points="0.75">half points</result>`
/// `<result outcome="partially-correct" pctype="50">points</result>`
pub fn parse_appes_xml(xml: &str, pe_as_wa: bool) -> Option<CheckResult> {
    // IMPORTANT: Use `to_ascii_lowercase` (never `to_lowercase`) so byte lengths and UTF-8
    // character boundaries are 100% preserved even when the checker message contains Unicode
    // characters whose lowercase expansion changes byte length (e.g. 'İ' U+0130 or 'K' U+212A).
    let lower = xml.to_ascii_lowercase();
    let start_tag = lower.find("<result")?;
    let end_tag_close = xml.get(start_tag..)?.find('>')? + start_tag;
    let tag_header = xml.get(start_tag..=end_tag_close)?;

    // Extract attributes
    let outcome = extract_attr(tag_header, "outcome")?;
    let points_str = extract_attr(tag_header, "points");
    let pctype_str = extract_attr(tag_header, "pctype");

    // Extract message body between <result...> and </result> (unless self-closing <result .../>)
    let self_closing = tag_header
        .strip_suffix('>')
        .is_some_and(|before_gt| before_gt.trim_end().ends_with('/'));
    let raw_msg = if self_closing {
        ""
    } else {
        let body_start = end_tag_close + 1;
        let body_end = lower
            .get(body_start..)
            .and_then(|s| s.find("</result>"))
            .map(|idx| body_start + idx)
            .unwrap_or(xml.len());
        xml.get(body_start..body_end).unwrap_or("").trim()
    };
    let message = if raw_msg.is_empty() {
        None
    } else {
        let unescaped = unescape_xml(raw_msg);
        Some(clip_str(&unescaped, MAX_CHECKER_MESSAGE_CHARS).0)
    };

    let outcome_norm = outcome.trim().to_ascii_lowercase();
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
            let parsed_ratio = if let Some(ref p_str) = points_str {
                p_str
                    .trim()
                    .trim_end_matches('%')
                    .parse::<f64>()
                    .ok()
                    .and_then(normalize_testlib_points)
            } else if let Some(ref pc_str) = pctype_str {
                match pc_str.trim().parse::<f64>() {
                    Ok(v) if v.is_finite() && (0.0..=100.0).contains(&v) => {
                        Some((v / 100.0).clamp(0.0, 1.0))
                    }
                    _ => None,
                }
            } else {
                Some(0.0)
            };

            let Some(ratio) = parsed_ratio else {
                return Some(CheckResult {
                    verdict: Verdict::JudgeFailed,
                    score_ratio: 0.0,
                    message: Some(
                        "Invalid or out-of-range points value in testlib XML".to_string(),
                    ),
                    diff: None,
                });
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
///
/// Notes on `_pc(pctype)` partial credit exit codes:
/// - In `testlib.h`, `TResult::_partially` is 16 and `_pc(n)` is `TResult(16 + n)`. When quitting
///   with `_pc(n)`, `testlib.h` prints `"partially correct (<n>) <msg>"` to `stderr` and exits with
///   `PC_BASE_EXIT_CODE + n` (where `PC_BASE_EXIT_CODE` defaults to 0 in vanilla testlib, 50 under
///   `TESTSYS`, or 16 when compiled with `-DPC_BASE_EXIT_CODE=16`).
/// - We first check `stderr` for `"partially correct (<n>)"` so `_pc(n)` is decoded accurately
///   regardless of `PC_BASE_EXIT_CODE`.
/// - Otherwise, exit codes `16..=116` are interpreted as `(code - 16) / 100.0` (the offset-16
///   convention for `0%..=100%`). Exit codes `> 116` (e.g. `128 + signal` or `255` from `exit(-1)`)
///   are treated as [`Verdict::JudgeFailed`] rather than being clamped to `Accepted`.
pub fn parse_testlib_exit_code(code: i32, stderr: &str, pe_as_wa: bool) -> CheckResult {
    let msg = stderr.trim();
    let message = if msg.is_empty() {
        None
    } else {
        Some(clip_str(msg, MAX_CHECKER_MESSAGE_CHARS).0)
    };

    // If testlib emitted a `partially correct (<pctype>)` diagnostic on stderr (from `quitf(_pc(n), ...)`),
    // honour `<pctype>` directly unless the exit code explicitly indicates a hard failure (3).
    if code != 3
        && let Some(ratio) = parse_partially_correct_from_stderr(stderr)
    {
        let verdict = if (ratio - 1.0).abs() < 1e-6 {
            Verdict::Accepted
        } else if ratio <= 0.0 {
            Verdict::WrongAnswer
        } else {
            Verdict::PartiallyCorrect
        };
        return CheckResult {
            verdict,
            score_ratio: ratio,
            message,
            diff: None,
        };
    }

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
            // First check if stderr itself contains -appes XML
            if stderr.to_ascii_lowercase().contains("<result")
                && let Some(res) = parse_appes_xml(stderr, pe_as_wa)
            {
                return res;
            }

            if let Some((ratio, parsed_msg)) = parse_points_from_stderr(stderr) {
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
                    message: parsed_msg
                        .map(|m| clip_str(&m, MAX_CHECKER_MESSAGE_CHARS).0)
                        .or(message),
                    diff: None,
                }
            } else {
                let stderr_clipped = clip_str(stderr.trim(), MAX_CHECKER_MESSAGE_CHARS).0;
                CheckResult {
                    verdict: Verdict::JudgeFailed,
                    score_ratio: 0.0,
                    message: Some(format!(
                        "Checker exited with code 7 (points) but failed to parse points from output: {stderr_clipped}"
                    )),
                    diff: None,
                }
            }
        }
        8 => CheckResult {
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message: message.or_else(|| Some("Unexpected end of file".to_string())),
            diff: None,
        },
        16..=116 => {
            // Partially correct with offset 16 (0%..=100%)
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
    let result_file = match reserve_scratch_file(&scratch_dir, "testlib_res", "xml") {
        Ok(p) => p,
        Err(e) => {
            return CheckResult {
                verdict: Verdict::JudgeFailed,
                score_ratio: 0.0,
                message: Some(format!(
                    "Failed to reserve testlib scratch result file: {e}"
                )),
                diff: None,
            };
        }
    };

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

    // 1. Try to read and parse -appes XML result file (using lossy UTF-8 decoding because
    // testlib declares windows-1251 and may emit non-UTF-8 bytes in its message).
    let check_result = if result_file.is_file() {
        if let Ok(xml_bytes) = fs::read(&result_file) {
            let xml_content = String::from_utf8_lossy(&xml_bytes);
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
    // Look for pattern: attr_name\s*=\s*"value" (case-insensitive ASCII attribute name)
    let tag_lower = tag.to_ascii_lowercase();
    let attr_lower = attr_name.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(idx) = tag_lower.get(search_from..)?.find(&attr_lower) {
        let abs_idx = search_from + idx;
        let valid_boundary = abs_idx == 0
            || tag_lower.as_bytes()[abs_idx - 1].is_ascii_whitespace()
            || matches!(tag_lower.as_bytes()[abs_idx - 1], b'<' | b'"' | b'\'');
        if valid_boundary {
            let rest = tag.get(abs_idx + attr_lower.len()..)?;
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
        }
        search_from = abs_idx + attr_lower.len();
    }
    None
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&#9;", "\t")
        .replace("&amp;", "&")
}

fn parse_partially_correct_from_stderr(stderr: &str) -> Option<f64> {
    let lower = stderr.trim_start().to_ascii_lowercase();
    let rest = lower.strip_prefix("partially correct")?;
    let rest = rest.trim_start().strip_prefix('(')?;
    let close = rest.find(')')?;
    let num_str = rest[..close].trim().trim_end_matches('%');
    let val = num_str.parse::<f64>().ok()?;
    if val.is_finite() && (0.0..=100.0).contains(&val) {
        Some((val / 100.0).clamp(0.0, 1.0))
    } else {
        None
    }
}

fn parse_points_from_stderr(stderr: &str) -> Option<(f64, Option<String>)> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let lower = word.to_ascii_lowercase();
        let stripped = lower.trim_end_matches([':', '=']);
        if stripped == "points" || stripped == "point" {
            let num_str = if let Some(pos) = word.find([':', '=']) {
                let rest = &word[pos + 1..];
                if !rest.is_empty() {
                    Some(rest)
                } else {
                    words.get(i + 1).copied()
                }
            } else {
                words.get(i + 1).copied()
            };

            if let Some(candidate) = num_str {
                let cleaned = candidate.trim_end_matches(['%', ',', ';']);
                if let Ok(val) = cleaned.parse::<f64>()
                    && let Some(ratio) = normalize_testlib_points(val)
                {
                    return Some((ratio, Some(trimmed.to_string())));
                }
            }
        }
    }

    if let Some(first_word) = words.first() {
        let cleaned = first_word.trim_end_matches(['%', ',', ';', ':']);
        if let Ok(val) = cleaned.parse::<f64>()
            && let Some(ratio) = normalize_testlib_points(val)
        {
            return Some((ratio, Some(trimmed.to_string())));
        }
    }

    None
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
    fn test_parse_appes_nan_points() {
        let xml_nan = "<result outcome=\"points\" points=\"NaN\">corrupted NaN</result>";
        let res_nan = parse_appes_xml(xml_nan, true).unwrap();
        assert_eq!(res_nan.verdict, Verdict::JudgeFailed);
        assert_eq!(res_nan.score_ratio, 0.0);

        let xml_inf = "<result outcome=\"points\" points=\"Infinity\">overflow Inf</result>";
        let res_inf = parse_appes_xml(xml_inf, true).unwrap();
        assert_eq!(res_inf.verdict, Verdict::JudgeFailed);
        assert_eq!(res_inf.score_ratio, 0.0);
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

        // Exit 7 with points keyword
        let res7_kw = parse_testlib_exit_code(7, "points 0.75 Correct subset", true);
        assert_eq!(res7_kw.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res7_kw.score_ratio, 0.75);

        // Exit 7 with points percentage
        let res7_pct = parse_testlib_exit_code(7, "points: 40 40%", true);
        assert_eq!(res7_pct.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res7_pct.score_ratio, 0.40);

        // Exit 7 with leading float
        let res7_float = parse_testlib_exit_code(7, "0.85 Good answer", true);
        assert_eq!(res7_float.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res7_float.score_ratio, 0.85);

        // Exit 7 with embedded XML in stderr
        let res7_xml = parse_testlib_exit_code(
            7,
            "<result outcome=\"points\" points=\"0.65\">xml in stderr</result>",
            true,
        );
        assert_eq!(res7_xml.verdict, Verdict::PartiallyCorrect);
        assert_eq!(res7_xml.score_ratio, 0.65);

        // Exit 7 with unparseable points -> JudgeFailed
        let res7_fail = parse_testlib_exit_code(7, "something crashed without points", true);
        assert_eq!(res7_fail.verdict, Verdict::JudgeFailed);
        assert_eq!(res7_fail.score_ratio, 0.0);

        // Exit code > 116 (e.g. 255 from exit(-1) or 139 from SIGSEGV) must be JudgeFailed, never Accepted
        assert_eq!(
            parse_testlib_exit_code(255, "fatal error", true).verdict,
            Verdict::JudgeFailed
        );
        assert_eq!(
            parse_testlib_exit_code(139, "segmentation fault", true).verdict,
            Verdict::JudgeFailed
        );

        // testlib.h `quitf(_pc(40), ...)` stderr format works regardless of PC_BASE_EXIT_CODE (0, 16, or 50)
        let pc_stderr = parse_testlib_exit_code(40, "partially correct (40) result=40", true);
        assert_eq!(pc_stderr.verdict, Verdict::PartiallyCorrect);
        assert!((pc_stderr.score_ratio - 0.40).abs() < 1e-6);
    }

    #[test]
    fn test_parse_appes_unicode_case_expansion_no_panic() {
        // 'İ' (U+0130, 2 UTF-8 bytes) expands to 3 bytes under Unicode `to_lowercase()`,
        // and Kelvin sign 'K' (U+212A, 3 UTF-8 bytes) shrinks to 1 byte ('k').
        // Neither must shift byte offsets or panic when slicing `<result>...</result>`.
        let dotted_i = "İ".repeat(25);
        let xml_i = format!(
            "<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n<RESULT OUTCOME=\"wrong-answer\">found: '{dotted_i}'</RESULT>"
        );
        let res_i = parse_appes_xml(&xml_i, true).expect("must parse XML with İ without panicking");
        assert_eq!(res_i.verdict, Verdict::WrongAnswer);
        assert_eq!(
            res_i.message.as_deref(),
            Some(format!("found: '{dotted_i}'").as_str())
        );

        let kelvin = "K".repeat(25);
        let xml_k = format!("<result outcome=\"wrong-answer\">unit: '{kelvin}'</result>");
        let res_k =
            parse_appes_xml(&xml_k, true).expect("must parse XML with Kelvin sign without panic");
        assert_eq!(res_k.verdict, Verdict::WrongAnswer);
        assert_eq!(
            res_k.message.as_deref(),
            Some(format!("unit: '{kelvin}'").as_str())
        );

        // Windows-1251 raw bytes decoded via `from_utf8_lossy` must still parse cleanly
        let raw_cp1251 = b"<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n<result outcome=\"wrong-answer\">\xce\xf8\xe8\xe1\xea\xe0</result>\n";
        let lossy = String::from_utf8_lossy(raw_cp1251);
        let res_cp1251 = parse_appes_xml(&lossy, true).expect("must parse lossy windows-1251 XML");
        assert_eq!(res_cp1251.verdict, Verdict::WrongAnswer);
    }

    #[test]
    fn test_points_scale_consistency_between_xml_and_stderr() {
        // Percentage points (50 -> 0.50) must behave identically in XML and exit-code-7 stderr
        let xml_50 = parse_appes_xml(
            "<result outcome=\"points\" points=\"50\">half credit</result>",
            true,
        )
        .unwrap();
        let stderr_50 = parse_testlib_exit_code(7, "points 50 half credit", true);
        assert_eq!(xml_50.verdict, Verdict::PartiallyCorrect);
        assert_eq!(stderr_50.verdict, Verdict::PartiallyCorrect);
        assert!((xml_50.score_ratio - 0.50).abs() < 1e-6);
        assert!((stderr_50.score_ratio - 0.50).abs() < 1e-6);

        // Out-of-range points (> 100 or < 0) must be rejected as JudgeFailed
        let xml_oob = parse_appes_xml(
            "<result outcome=\"points\" points=\"150\">too high</result>",
            true,
        )
        .unwrap();
        assert_eq!(xml_oob.verdict, Verdict::JudgeFailed);
    }
}

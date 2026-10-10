//! Built-in checkers (exact, wcmp, fcmp, lcmp, ncmp, hcmp, rcmp4/6/9, yesno).

use crate::{CheckResult, DiffSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCheckerType {
    Exact,
    Wcmp,
    Fcmp,
    Lcmp,
    Ncmp,
    Hcmp,
    Rcmp4,
    Rcmp6,
    Rcmp9,
    Yesno,
}

impl BuiltinCheckerType {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "exact" => Some(Self::Exact),
            "wcmp" => Some(Self::Wcmp),
            "fcmp" => Some(Self::Fcmp),
            "lcmp" => Some(Self::Lcmp),
            "ncmp" => Some(Self::Ncmp),
            "hcmp" => Some(Self::Hcmp),
            "rcmp4" => Some(Self::Rcmp4),
            "rcmp6" => Some(Self::Rcmp6),
            "rcmp9" => Some(Self::Rcmp9),
            "yesno" => Some(Self::Yesno),
            _ => None,
        }
    }

    pub fn check(&self, output: &[u8], answer: &[u8]) -> CheckResult {
        match self {
            Self::Exact => check_exact(output, answer),
            Self::Wcmp => check_wcmp(output, answer),
            Self::Fcmp => check_fcmp(output, answer),
            Self::Lcmp => check_lcmp(output, answer),
            Self::Ncmp => check_ncmp(output, answer),
            Self::Hcmp => check_hcmp(output, answer),
            Self::Rcmp4 => check_rcmp(output, answer, 1e-4),
            Self::Rcmp6 => check_rcmp(output, answer, 1e-6),
            Self::Rcmp9 => check_rcmp(output, answer, 1e-9),
            Self::Yesno => check_yesno(output, answer),
        }
    }
}

// ----------------------------------------------------------------------------
// Tokenizer
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub slice: &'a [u8],
    pub line: usize,
    pub col: usize,
    pub index: usize,
}

impl<'a> Token<'a> {
    pub fn as_str(&self) -> &'a str {
        std::str::from_utf8(self.slice).unwrap_or("<invalid-utf8>")
    }

    pub fn snippet(&self, max_len: usize) -> String {
        let s = self.as_str();
        if s.len() > max_len {
            format!("{}...", &s[..max_len])
        } else {
            s.to_string()
        }
    }
}

pub struct TokenIterator<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
    token_index: usize,
}

impl<'a> TokenIterator<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            line: 1,
            col: 1,
            token_index: 0,
        }
    }
}

impl<'a> Iterator for TokenIterator<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // Skip whitespace
        while self.pos < self.input.len() {
            // Fast path: skip 8 consecutive spaces (0x20)
            while self.pos + 8 <= self.input.len() {
                let chunk =
                    u64::from_le_bytes(self.input[self.pos..self.pos + 8].try_into().unwrap());
                if chunk == 0x2020202020202020 {
                    self.pos += 8;
                    self.col += 8;
                } else {
                    break;
                }
            }
            if self.pos >= self.input.len() {
                break;
            }

            let b = self.input[self.pos];
            if b == b'\n' {
                self.line += 1;
                self.col = 1;
                self.pos += 1;
            } else if b.is_ascii_whitespace() {
                self.col += 1;
                self.pos += 1;
            } else {
                break;
            }
        }

        if self.pos >= self.input.len() {
            return None;
        }

        let start_pos = self.pos;
        let start_line = self.line;
        let start_col = self.col;

        // Fast path: scan 8 non-whitespace ASCII characters at a time using SWAR
        while self.pos + 8 <= self.input.len() {
            let chunk = u64::from_le_bytes(self.input[self.pos..self.pos + 8].try_into().unwrap());
            // Break if any byte has high bit set (non-ASCII >= 0x80)
            if (chunk & 0x8080808080808080) != 0 {
                break;
            }
            // If any byte <= 0x20 (ASCII whitespace / control), (chunk - 0x2121...) will borrow to high bit
            let sub = chunk.wrapping_sub(0x2121212121212121);
            if (sub & 0x8080808080808080) != 0 {
                break;
            }
            self.pos += 8;
            self.col += 8;
        }

        // Tail: standard byte-by-byte scan
        while self.pos < self.input.len() {
            let b = self.input[self.pos];
            if b.is_ascii_whitespace() {
                break;
            }
            self.pos += 1;
            self.col += 1;
        }

        self.token_index += 1;
        Some(Token {
            slice: &self.input[start_pos..self.pos],
            line: start_line,
            col: start_col,
            index: self.token_index,
        })
    }
}

// ----------------------------------------------------------------------------
// Builtin Checkers
// ----------------------------------------------------------------------------

/// Byte-by-byte exact match.
pub fn check_exact(output: &[u8], answer: &[u8]) -> CheckResult {
    if output == answer {
        return CheckResult::accepted();
    }

    let min_len = output.len().min(answer.len());
    let mut line = 1;
    let mut col = 1;
    let mut diff_pos = min_len;

    for i in 0..min_len {
        if output[i] != answer[i] {
            diff_pos = i;
            break;
        }
        if output[i] == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }

    let actual_snippet = if diff_pos < output.len() {
        let end = (diff_pos + 40).min(output.len());
        String::from_utf8_lossy(&output[diff_pos..end]).to_string()
    } else {
        "<EOF>".to_string()
    };

    let expected_snippet = if diff_pos < answer.len() {
        let end = (diff_pos + 40).min(answer.len());
        String::from_utf8_lossy(&answer[diff_pos..end]).to_string()
    } else {
        "<EOF>".to_string()
    };

    CheckResult::wrong_answer(
        format!("Exact mismatch at byte {diff_pos} (line {line}, col {col})"),
        Some(DiffSummary {
            line,
            column: col,
            token_index: 0,
            expected: expected_snippet,
            actual: actual_snippet,
        }),
    )
}

/// Token-by-token comparison (wcmp). Default in competitive programming.
pub fn check_wcmp(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = TokenIterator::new(output);
    let mut ans_iter = TokenIterator::new(answer);

    loop {
        match (out_iter.next(), ans_iter.next()) {
            (Some(out_tok), Some(ans_tok)) => {
                if out_tok.slice != ans_tok.slice {
                    return CheckResult::wrong_answer(
                        format!(
                            "Token mismatch at #{} (line {}, col {})",
                            ans_tok.index, out_tok.line, out_tok.col
                        ),
                        Some(DiffSummary {
                            line: out_tok.line,
                            column: out_tok.col,
                            token_index: ans_tok.index,
                            expected: ans_tok.snippet(100),
                            actual: out_tok.snippet(100),
                        }),
                    );
                }
            }
            (None, None) => return CheckResult::accepted(),
            (None, Some(ans_tok)) => {
                return CheckResult::wrong_answer(
                    format!("Unexpected EOF: missing expected token #{}", ans_tok.index),
                    Some(DiffSummary {
                        line: ans_tok.line,
                        column: ans_tok.col,
                        token_index: ans_tok.index,
                        expected: ans_tok.snippet(100),
                        actual: "<EOF>".to_string(),
                    }),
                );
            }
            (Some(out_tok), None) => {
                return CheckResult::wrong_answer(
                    format!(
                        "Extraneous token at #{} (line {}, col {})",
                        out_tok.index, out_tok.line, out_tok.col
                    ),
                    Some(DiffSummary {
                        line: out_tok.line,
                        column: out_tok.col,
                        token_index: out_tok.index,
                        expected: "<EOF>".to_string(),
                        actual: out_tok.snippet(100),
                    }),
                );
            }
        }
    }
}

/// Line-by-line comparison ignoring trailing whitespace and blank lines (fcmp).
pub fn check_fcmp(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = output.split(|&b| b == b'\n').map(strip_line_trailing);
    let mut ans_iter = answer.split(|&b| b == b'\n').map(strip_line_trailing);
    let mut line_num = 1;

    loop {
        let out_opt = out_iter.next();
        let ans_opt = ans_iter.next();

        match (out_opt, ans_opt) {
            (Some(out_line), Some(ans_line)) => {
                if out_line != ans_line {
                    return CheckResult::wrong_answer(
                        format!("Line mismatch at line {line_num}"),
                        Some(DiffSummary {
                            line: line_num,
                            column: 1,
                            token_index: 0,
                            expected: String::from_utf8_lossy(ans_line).to_string(),
                            actual: String::from_utf8_lossy(out_line).to_string(),
                        }),
                    );
                }
                line_num += 1;
            }
            (Some(out_line), None) => {
                if !out_line.is_empty() {
                    return CheckResult::wrong_answer(
                        format!("Line mismatch at line {line_num}"),
                        Some(DiffSummary {
                            line: line_num,
                            column: 1,
                            token_index: 0,
                            expected: String::new(),
                            actual: String::from_utf8_lossy(out_line).to_string(),
                        }),
                    );
                }
                line_num += 1;
            }
            (None, Some(ans_line)) => {
                if !ans_line.is_empty() {
                    return CheckResult::wrong_answer(
                        format!("Line mismatch at line {line_num}"),
                        Some(DiffSummary {
                            line: line_num,
                            column: 1,
                            token_index: 0,
                            expected: String::from_utf8_lossy(ans_line).to_string(),
                            actual: String::new(),
                        }),
                    );
                }
                line_num += 1;
            }
            (None, None) => return CheckResult::accepted(),
        }
    }
}

/// Line-by-line token comparison (lcmp).
pub fn check_lcmp(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = output.split(|&b| b == b'\n').map(strip_raw_line);
    let mut ans_iter = answer.split(|&b| b == b'\n').map(strip_raw_line);
    let mut line_num = 1;

    loop {
        let out_opt = out_iter.next();
        let ans_opt = ans_iter.next();

        if out_opt.is_none() && ans_opt.is_none() {
            break;
        }

        let out_line = out_opt.unwrap_or(b"");
        let ans_line = ans_opt.unwrap_or(b"");

        let mut out_tokens = TokenIterator::new(out_line);
        let mut ans_tokens = TokenIterator::new(ans_line);

        loop {
            match (out_tokens.next(), ans_tokens.next()) {
                (Some(o), Some(a)) => {
                    if o.slice != a.slice {
                        return CheckResult::wrong_answer(
                            format!("Line {}, token {} mismatch", line_num, a.index),
                            Some(DiffSummary {
                                line: line_num,
                                column: o.col,
                                token_index: a.index,
                                expected: a.snippet(100),
                                actual: o.snippet(100),
                            }),
                        );
                    }
                }
                (None, None) => break,
                (None, Some(a)) => {
                    return CheckResult::wrong_answer(
                        format!("Line {} missing expected token {}", line_num, a.index),
                        Some(DiffSummary {
                            line: line_num,
                            column: a.col,
                            token_index: a.index,
                            expected: a.snippet(100),
                            actual: "<EOF>".to_string(),
                        }),
                    );
                }
                (Some(o), None) => {
                    return CheckResult::wrong_answer(
                        format!("Line {} has unexpected token {}", line_num, o.index),
                        Some(DiffSummary {
                            line: line_num,
                            column: o.col,
                            token_index: o.index,
                            expected: "<EOL>".to_string(),
                            actual: o.snippet(100),
                        }),
                    );
                }
            }
        }

        line_num += 1;
    }

    CheckResult::accepted()
}

#[inline]
pub fn fast_parse_i64(slice: &[u8]) -> Option<i64> {
    if slice.is_empty() {
        return None;
    }
    let (is_negative, digits) = match slice[0] {
        b'-' => (true, &slice[1..]),
        b'+' => (false, &slice[1..]),
        _ => (false, slice),
    };
    if digits.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        let d = (b - b'0') as u64;
        val = val.checked_mul(10)?.checked_add(d)?;
    }
    if is_negative {
        if val > (i64::MAX as u64 + 1) {
            return None;
        }
        if val == (i64::MAX as u64 + 1) {
            Some(i64::MIN)
        } else {
            Some(-(val as i64))
        }
    } else {
        if val > i64::MAX as u64 {
            return None;
        }
        Some(val as i64)
    }
}

/// 64-bit signed integer comparison (ncmp).
pub fn check_ncmp(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = TokenIterator::new(output);
    let mut ans_iter = TokenIterator::new(answer);

    loop {
        match (out_iter.next(), ans_iter.next()) {
            (Some(out_tok), Some(ans_tok)) => {
                let ans_val = match fast_parse_i64(ans_tok.slice) {
                    Some(v) => v,
                    None => {
                        return CheckResult::wrong_answer(
                            format!(
                                "Answer contains invalid integer token '{}'",
                                ans_tok.as_str()
                            ),
                            None,
                        );
                    }
                };

                let out_val = match fast_parse_i64(out_tok.slice) {
                    Some(v) => v,
                    None => {
                        return CheckResult::wrong_answer(
                            format!(
                                "Output token #{} '{}' is not a valid 64-bit integer",
                                out_tok.index,
                                out_tok.as_str()
                            ),
                            Some(DiffSummary {
                                line: out_tok.line,
                                column: out_tok.col,
                                token_index: out_tok.index,
                                expected: ans_val.to_string(),
                                actual: out_tok.snippet(100),
                            }),
                        );
                    }
                };

                if out_val != ans_val {
                    return CheckResult::wrong_answer(
                        format!(
                            "Integer mismatch at #{} (expected {}, found {})",
                            ans_tok.index, ans_val, out_val
                        ),
                        Some(DiffSummary {
                            line: out_tok.line,
                            column: out_tok.col,
                            token_index: ans_tok.index,
                            expected: ans_val.to_string(),
                            actual: out_val.to_string(),
                        }),
                    );
                }
            }
            (None, None) => return CheckResult::accepted(),
            (None, Some(ans_tok)) => {
                return CheckResult::wrong_answer(
                    format!("Missing expected integer token #{}", ans_tok.index),
                    Some(DiffSummary {
                        line: ans_tok.line,
                        column: ans_tok.col,
                        token_index: ans_tok.index,
                        expected: ans_tok.snippet(100),
                        actual: "<EOF>".to_string(),
                    }),
                );
            }
            (Some(out_tok), None) => {
                return CheckResult::wrong_answer(
                    format!("Extraneous integer token at #{}", out_tok.index),
                    Some(DiffSummary {
                        line: out_tok.line,
                        column: out_tok.col,
                        token_index: out_tok.index,
                        expected: "<EOF>".to_string(),
                        actual: out_tok.snippet(100),
                    }),
                );
            }
        }
    }
}

/// Arbitrary length integer comparison (hcmp).
pub fn check_hcmp(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = TokenIterator::new(output);
    let mut ans_iter = TokenIterator::new(answer);

    loop {
        match (out_iter.next(), ans_iter.next()) {
            (Some(out_tok), Some(ans_tok)) => {
                let ans_norm = match normalize_bigint(ans_tok.slice) {
                    Ok(n) => n,
                    Err(_) => {
                        return CheckResult::wrong_answer(
                            format!(
                                "Answer contains invalid bigint token '{}'",
                                ans_tok.as_str()
                            ),
                            None,
                        );
                    }
                };

                let out_norm = match normalize_bigint(out_tok.slice) {
                    Ok(n) => n,
                    Err(_) => {
                        return CheckResult::wrong_answer(
                            format!(
                                "Output token #{} '{}' is not a valid integer",
                                out_tok.index,
                                out_tok.as_str()
                            ),
                            Some(DiffSummary {
                                line: out_tok.line,
                                column: out_tok.col,
                                token_index: out_tok.index,
                                expected: ans_norm.to_string(),
                                actual: out_tok.snippet(100),
                            }),
                        );
                    }
                };

                if out_norm != ans_norm {
                    let ans_str = ans_norm.to_string();
                    let out_str = out_norm.to_string();
                    return CheckResult::wrong_answer(
                        format!(
                            "BigInt mismatch at #{} (expected {}, found {})",
                            ans_tok.index, ans_str, out_str
                        ),
                        Some(DiffSummary {
                            line: out_tok.line,
                            column: out_tok.col,
                            token_index: ans_tok.index,
                            expected: ans_str,
                            actual: out_str,
                        }),
                    );
                }
            }
            (None, None) => return CheckResult::accepted(),
            (None, Some(ans_tok)) => {
                return CheckResult::wrong_answer(
                    format!("Missing expected bigint token #{}", ans_tok.index),
                    None,
                );
            }
            (Some(out_tok), None) => {
                return CheckResult::wrong_answer(
                    format!("Extraneous bigint token at #{}", out_tok.index),
                    None,
                );
            }
        }
    }
}

/// Floating point comparison with absolute or relative error (rcmp).
pub fn check_rcmp(output: &[u8], answer: &[u8], eps: f64) -> CheckResult {
    let mut out_iter = TokenIterator::new(output);
    let mut ans_iter = TokenIterator::new(answer);

    loop {
        match (out_iter.next(), ans_iter.next()) {
            (Some(out_tok), Some(ans_tok)) => {
                let ans_val = match ans_tok.as_str().parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        return CheckResult::wrong_answer(
                            format!("Answer contains invalid float token '{}'", ans_tok.as_str()),
                            None,
                        );
                    }
                };

                let out_val = match out_tok.as_str().parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        return CheckResult::wrong_answer(
                            format!(
                                "Output token #{} '{}' is not a valid float",
                                out_tok.index,
                                out_tok.as_str()
                            ),
                            Some(DiffSummary {
                                line: out_tok.line,
                                column: out_tok.col,
                                token_index: out_tok.index,
                                expected: ans_val.to_string(),
                                actual: out_tok.snippet(100),
                            }),
                        );
                    }
                };

                if ans_val.is_nan() {
                    if !out_val.is_nan() {
                        return CheckResult::wrong_answer("Expected NaN", None);
                    }
                } else if ans_val.is_infinite() {
                    if !out_val.is_infinite()
                        || (ans_val.is_sign_positive() != out_val.is_sign_positive())
                    {
                        return CheckResult::wrong_answer(
                            format!("Expected {}, found {}", ans_val, out_val),
                            None,
                        );
                    }
                } else {
                    let diff = (ans_val - out_val).abs();
                    let denom = ans_val.abs().max(out_val.abs());
                    let close_abs = diff <= eps;
                    let close_rel = denom > 0.0 && (diff / denom) <= eps;

                    if !close_abs && !close_rel {
                        return CheckResult::wrong_answer(
                            format!(
                                "Float mismatch at #{} (expected {}, found {}, diff {})",
                                ans_tok.index, ans_val, out_val, diff
                            ),
                            Some(DiffSummary {
                                line: out_tok.line,
                                column: out_tok.col,
                                token_index: ans_tok.index,
                                expected: ans_val.to_string(),
                                actual: out_val.to_string(),
                            }),
                        );
                    }
                }
            }
            (None, None) => return CheckResult::accepted(),
            (None, Some(ans_tok)) => {
                return CheckResult::wrong_answer(
                    format!("Missing expected float token #{}", ans_tok.index),
                    None,
                );
            }
            (Some(out_tok), None) => {
                return CheckResult::wrong_answer(
                    format!("Extraneous float token at #{}", out_tok.index),
                    None,
                );
            }
        }
    }
}

/// Single token YES/NO checker (yesno, case-insensitive).
pub fn check_yesno(output: &[u8], answer: &[u8]) -> CheckResult {
    let mut out_iter = TokenIterator::new(output);
    let mut ans_iter = TokenIterator::new(answer);

    let ans_tok = match ans_iter.next() {
        Some(t) => t,
        None => return CheckResult::wrong_answer("Answer file is empty", None),
    };

    if ans_iter.next().is_some() {
        return CheckResult::wrong_answer("Answer file has more than one token", None);
    }

    let out_tok = match out_iter.next() {
        Some(t) => t,
        None => return CheckResult::wrong_answer("Output file is empty", None),
    };

    if out_iter.next().is_some() {
        return CheckResult::wrong_answer("Output file has more than one token", None);
    }

    let ans_lower = ans_tok.as_str().to_lowercase();
    let out_lower = out_tok.as_str().to_lowercase();

    if ans_lower != "yes" && ans_lower != "no" {
        return CheckResult::wrong_answer(
            format!(
                "Invalid token in answer: '{}' (must be yes or no)",
                ans_tok.as_str()
            ),
            None,
        );
    }

    if out_lower != "yes" && out_lower != "no" {
        return CheckResult::wrong_answer(
            format!(
                "Invalid token in output: '{}' (must be yes or no)",
                out_tok.as_str()
            ),
            Some(DiffSummary {
                line: out_tok.line,
                column: out_tok.col,
                token_index: 1,
                expected: ans_tok.as_str().to_string(),
                actual: out_tok.as_str().to_string(),
            }),
        );
    }

    if ans_lower == out_lower {
        CheckResult::accepted()
    } else {
        CheckResult::wrong_answer(
            format!(
                "Expected '{}', found '{}'",
                ans_tok.as_str(),
                out_tok.as_str()
            ),
            Some(DiffSummary {
                line: out_tok.line,
                column: out_tok.col,
                token_index: 1,
                expected: ans_tok.as_str().to_string(),
                actual: out_tok.as_str().to_string(),
            }),
        )
    }
}

// ----------------------------------------------------------------------------
// Internal Helpers
// ----------------------------------------------------------------------------

#[inline]
fn strip_line_trailing(mut line: &[u8]) -> &[u8] {
    while let Some(&last) = line.last() {
        if last == b'\r' || last == b' ' || last == b'\t' {
            line = &line[..line.len() - 1];
        } else {
            break;
        }
    }
    line
}

#[inline]
fn strip_raw_line(mut line: &[u8]) -> &[u8] {
    if let Some(&b'\r') = line.last() {
        line = &line[..line.len() - 1];
    }
    line
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedBigInt<'a> {
    pub negative: bool,
    pub digits: &'a [u8],
}

impl<'a> std::fmt::Display for NormalizedBigInt<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.negative {
            write!(f, "-")?;
        }
        let s = std::str::from_utf8(self.digits).unwrap_or("<invalid>");
        write!(f, "{s}")
    }
}

fn normalize_bigint(bytes: &[u8]) -> Result<NormalizedBigInt<'_>, ()> {
    if bytes.is_empty() {
        return Err(());
    }

    let mut start = 0;
    let mut negative = false;

    if bytes[0] == b'-' {
        negative = true;
        start = 1;
    } else if bytes[0] == b'+' {
        start = 1;
    }

    if start >= bytes.len() {
        return Err(());
    }

    // Validate digits
    for &b in &bytes[start..] {
        if !b.is_ascii_digit() {
            return Err(());
        }
    }

    // Skip leading zeros
    while start < bytes.len() && bytes[start] == b'0' {
        start += 1;
    }

    if start == bytes.len() {
        // Number is 0 (or -0, +000)
        return Ok(NormalizedBigInt {
            negative: false,
            digits: b"0",
        });
    }

    Ok(NormalizedBigInt {
        negative,
        digits: &bytes[start..],
    })
}

// ----------------------------------------------------------------------------
// Unit Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact() {
        assert_eq!(
            check_exact(b"1 2 3\n", b"1 2 3\n").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_exact(b"1 2 3\r\n", b"1 2 3\n").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_wcmp() {
        assert_eq!(
            check_wcmp(b"  1 \r\n 2 \t 3 \n", b"1 2 3").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_wcmp(b"1 2 4", b"1 2 3").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_wcmp(b"1 2", b"1 2 3").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_wcmp(b"1 2 3 4", b"1 2 3").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_fcmp() {
        assert_eq!(
            check_fcmp(b"line 1  \r\nline 2\r\n\r\n", b"line 1\nline 2").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_fcmp(b"line 1\nline 3", b"line 1\nline 2").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_lcmp() {
        assert_eq!(
            check_lcmp(b"1 2\r\n3 4\r\n", b"1  2\n3 4").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_lcmp(b"1 2 3\n4", b"1 2\n3 4").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_ncmp() {
        assert_eq!(
            check_ncmp(b"001 -005 9223372036854775807", b"1 -5 9223372036854775807").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_ncmp(b"1 2 3", b"1 2 4").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_ncmp(b"abc", b"123").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_hcmp() {
        assert_eq!(
            check_hcmp(
                b"+000 -0 0012345678901234567890",
                b"0 0 12345678901234567890"
            )
            .verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_hcmp(b"-0001", b"-1").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_hcmp(b"123456789012345678901", b"123456789012345678902").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_rcmp() {
        assert_eq!(
            check_rcmp(b"3.14159265", b"3.14159265", 1e-6).verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_rcmp(b"3.141592", b"3.141593", 1e-5).verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_rcmp(b"3.141592", b"3.141593", 1e-7).verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_rcmp(b"NaN", b"NaN", 1e-6).verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_rcmp(b"inf", b"inf", 1e-6).verdict,
            uj_core::verdict::Verdict::Accepted
        );
    }

    #[test]
    fn test_yesno() {
        assert_eq!(
            check_yesno(b"Yes", b"YES").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_yesno(b"no", b"No").verdict,
            uj_core::verdict::Verdict::Accepted
        );
        assert_eq!(
            check_yesno(b"yes", b"no").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_yesno(b"maybe", b"yes").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
        assert_eq!(
            check_yesno(b"yes yes", b"yes").verdict,
            uj_core::verdict::Verdict::WrongAnswer
        );
    }

    #[test]
    fn test_fast_parse_i64() {
        assert_eq!(super::fast_parse_i64(b"0"), Some(0));
        assert_eq!(super::fast_parse_i64(b"+0"), Some(0));
        assert_eq!(super::fast_parse_i64(b"-0"), Some(0));
        assert_eq!(super::fast_parse_i64(b"42"), Some(42));
        assert_eq!(super::fast_parse_i64(b"+42"), Some(42));
        assert_eq!(super::fast_parse_i64(b"-42"), Some(-42));
        assert_eq!(
            super::fast_parse_i64(b"9223372036854775807"),
            Some(i64::MAX)
        );
        assert_eq!(
            super::fast_parse_i64(b"-9223372036854775808"),
            Some(i64::MIN)
        );
        // Overflow
        assert_eq!(super::fast_parse_i64(b"9223372036854775808"), None);
        assert_eq!(super::fast_parse_i64(b"-9223372036854775809"), None);
        assert_eq!(super::fast_parse_i64(b"100000000000000000000"), None);
        // Invalid formats
        assert_eq!(super::fast_parse_i64(b""), None);
        assert_eq!(super::fast_parse_i64(b"+"), None);
        assert_eq!(super::fast_parse_i64(b"-"), None);
        assert_eq!(super::fast_parse_i64(b"12a34"), None);
        assert_eq!(super::fast_parse_i64(b" 42"), None);
    }

    #[test]
    fn test_swar_tokenizer() {
        // Words of varying lengths (>= 8, < 8, exact 8, multiple 8s)
        let input = b"        12345678  123456789  1234567  abcdefghijklmnop  \n  \t  utf8_\xe4\xbd\xa0\xe5\xa5\xbd_test  end";
        let tokens: Vec<_> = TokenIterator::new(input)
            .map(|t| (t.as_str().to_string(), t.line, t.col))
            .collect();
        assert_eq!(
            tokens,
            vec![
                ("12345678".to_string(), 1, 9),
                ("123456789".to_string(), 1, 19),
                ("1234567".to_string(), 1, 30),
                ("abcdefghijklmnop".to_string(), 1, 39),
                ("utf8_\u{4f60}\u{597d}_test".to_string(), 2, 6),
                ("end".to_string(), 2, 24),
            ]
        );
    }
}

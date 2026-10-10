use crate::config::{ProblemConfig, ScoringType};
use globset::Glob;
use regex::Regex;
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

/// A single discovered test case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub name: String,
    pub input_path: PathBuf,
    pub answer_path: Option<PathBuf>,
    pub subtask_id: Option<u32>,
    /// Base points allocated to this test case (scaled by 1e6 for fixed-point integer precision).
    pub weight: u64,
}

/// Natural sort comparison for strings containing numbers (e.g., "test2" < "test10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_chars = a.chars().peekable();
    let mut b_chars = b.chars().peekable();

    while let (Some(&ca), Some(&cb)) = (a_chars.peek(), b_chars.peek()) {
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Parse full number sequences
            let mut num_a: u128 = 0;
            while let Some(&c) = a_chars.peek() {
                if let Some(d) = c.to_digit(10) {
                    num_a = num_a.saturating_mul(10).saturating_add(d as u128);
                    a_chars.next();
                } else {
                    break;
                }
            }

            let mut num_b: u128 = 0;
            while let Some(&c) = b_chars.peek() {
                if let Some(d) = c.to_digit(10) {
                    num_b = num_b.saturating_mul(10).saturating_add(d as u128);
                    b_chars.next();
                } else {
                    break;
                }
            }

            match num_a.cmp(&num_b) {
                Ordering::Equal => continue,
                other => return other,
            }
        } else {
            match ca.cmp(&cb) {
                Ordering::Equal => {
                    a_chars.next();
                    b_chars.next();
                }
                other => return other,
            }
        }
    }

    a_chars.peek().is_some().cmp(&b_chars.peek().is_some())
}

/// Discover tests from directory according to configuration.
pub fn discover_tests(
    problem_dir: &Path,
    config: &ProblemConfig,
) -> Result<Vec<TestCase>, std::io::Error> {
    let abs_problem_dir = if problem_dir.is_absolute() {
        problem_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(problem_dir)
    };
    let tests_dir = abs_problem_dir.join(&config.tests.dir);
    if !tests_dir.exists() {
        return Ok(Vec::new());
    }

    let mut found_inputs: Vec<(String, PathBuf)> = Vec::new();

    for entry in fs::read_dir(&tests_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && let Some(ext) = path.extension()
            && ext.eq_ignore_ascii_case("in")
        {
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            found_inputs.push((stem, path));
        }
    }

    // Natural sort the found inputs by their test name/stem
    found_inputs.sort_by(|a, b| natural_cmp(&a.0, &b.0));

    let mut cases = Vec::with_capacity(found_inputs.len());

    for (stem, in_path) in found_inputs {
        // Look for matching .ans or .out
        let ans_candidate = tests_dir.join(format!("{stem}.ans"));
        let out_candidate = tests_dir.join(format!("{stem}.out"));

        let answer_path = if ans_candidate.is_file() {
            Some(ans_candidate)
        } else if out_candidate.is_file() {
            Some(out_candidate)
        } else {
            None
        };

        cases.push(TestCase {
            name: stem,
            input_path: in_path,
            answer_path,
            subtask_id: None,
            weight: 0,
        });
    }

    // Map subtasks and distribute weights
    if config.subtasks.is_empty() {
        // Default: Evenly distribute 100 points among all test cases.
        // Remainder is distributed to the first remainder cases.
        let n = cases.len();
        if n > 0 {
            let total_fixed = 100 * 1_000_000u64;
            let base = total_fixed / (n as u64);
            let rem = total_fixed % (n as u64);
            for (idx, case) in cases.iter_mut().enumerate() {
                case.weight = base + if (idx as u64) < rem { 1 } else { 0 };
            }
        }
    } else {
        // Assign test cases to subtasks based on patterns
        for sub in &config.subtasks {
            let pattern_set = PatternSet::compile(&sub.tests);
            let sub_total_fixed = (sub.score * 1_000_000.0).round() as u64;
            let mut matched_indices = Vec::new();

            for (idx, case) in cases.iter().enumerate() {
                if pattern_set.is_match(&case.name) {
                    matched_indices.push(idx);
                }
            }

            let m = matched_indices.len();
            if m > 0 {
                let base = sub_total_fixed / (m as u64);
                let rem = sub_total_fixed % (m as u64);
                for (rank, &idx) in matched_indices.iter().enumerate() {
                    cases[idx].subtask_id = Some(sub.id);
                    if sub.scoring == ScoringType::Sum {
                        cases[idx].weight = base + if (rank as u64) < rem { 1 } else { 0 };
                    } else {
                        // In min scoring, the subtask score is determined as min ratio * subtask_score
                        cases[idx].weight = sub_total_fixed;
                    }
                }
            }
        }
    }

    Ok(cases)
}

/// A parsed, reusable pattern for subtask test matching.
#[derive(Debug, Clone)]
pub enum CompiledPattern {
    Regex(Option<Regex>),
    NumericRange {
        start: u64,
        end: u64,
        exact_fallback: String,
    },
    Glob {
        matcher: Option<globset::GlobMatcher>,
        exact_fallback: String,
    },
    Exact(String),
}

impl CompiledPattern {
    pub fn parse(raw: &str) -> Self {
        let pat = raw.trim();

        // 1. Regex pattern
        if let Some(regex_str) = pat.strip_prefix("re:") {
            let re = Regex::new(regex_str).ok();
            return Self::Regex(re);
        }

        // 2. Numeric range pattern like "1-10"
        if let Some((start_s, end_s)) = pat.split_once('-')
            && let (Ok(start), Ok(end)) =
                (start_s.trim().parse::<u64>(), end_s.trim().parse::<u64>())
        {
            return Self::NumericRange {
                start,
                end,
                exact_fallback: pat.to_string(),
            };
        }

        // 3. Glob pattern
        if pat.contains('*') || pat.contains('?') || pat.contains('[') {
            let matcher = Glob::new(pat).ok().map(|g| g.compile_matcher());
            return Self::Glob {
                matcher,
                exact_fallback: pat.to_string(),
            };
        }

        // 4. Exact string match
        Self::Exact(pat.to_string())
    }

    pub fn is_match(&self, stem: &str) -> bool {
        match self {
            Self::Regex(Some(re)) => re.is_match(stem),
            Self::Regex(None) => false,
            Self::NumericRange {
                start,
                end,
                exact_fallback,
            } => {
                if let Ok(val) = stem.parse::<u64>()
                    && (*start..=*end).contains(&val)
                {
                    return true;
                }
                exact_fallback == stem
            }
            Self::Glob {
                matcher,
                exact_fallback,
            } => {
                if let Some(m) = matcher
                    && m.is_match(stem)
                {
                    return true;
                }
                exact_fallback == stem
            }
            Self::Exact(exact) => exact == stem,
        }
    }
}

/// A precompiled set of patterns for a subtask.
#[derive(Debug, Clone, Default)]
pub struct PatternSet {
    patterns: Vec<CompiledPattern>,
}

impl PatternSet {
    pub fn compile(patterns: &[String]) -> Self {
        let compiled = patterns.iter().map(|p| CompiledPattern::parse(p)).collect();
        Self { patterns: compiled }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn is_match(&self, stem: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        for pat in &self.patterns {
            if pat.is_match(stem) {
                return true;
            }
        }
        false
    }
}

/// Check if a test stem matches any of the given pattern specifications.
/// Supported patterns:
/// - Range: "1-10"
/// - Regex: "re:^small"
/// - Glob: "sample*"
/// - Exact string: "sample1"
pub fn test_matches_patterns(stem: &str, patterns: &[String]) -> bool {
    PatternSet::compile(patterns).is_match(stem)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_natural_sort() {
        let mut tests = vec!["test10", "test2", "test1", "test20", "test3"];
        tests.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(tests, vec!["test1", "test2", "test3", "test10", "test20"]);
    }

    #[test]
    fn test_pattern_matching() {
        let patterns = vec![
            "1-10".to_string(),
            "sample*".to_string(),
            "re:^edge_\\d+$".to_string(),
        ];
        assert!(test_matches_patterns("5", &patterns));
        assert!(test_matches_patterns("1", &patterns));
        assert!(test_matches_patterns("10", &patterns));
        assert!(!test_matches_patterns("11", &patterns));
        assert!(test_matches_patterns("sample1", &patterns));
        assert!(test_matches_patterns("edge_42", &patterns));
        assert!(!test_matches_patterns("edge_abc", &patterns));

        // Exact match fallback for range string itself
        assert!(test_matches_patterns("1-10", &["1-10".to_string()]));

        // Illegal regex should not match and should not panic
        let bad_regex = vec!["re:[a-z".to_string()];
        assert!(!test_matches_patterns("abc", &bad_regex));

        // Exact fallback for special characters if glob compilation fails
        let bad_glob = vec!["[".to_string()];
        assert!(test_matches_patterns("[", &bad_glob));
        assert!(!test_matches_patterns("x", &bad_glob));
    }
}

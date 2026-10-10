//! uj-checker: Built-in comparison algorithms and testlib/Lemon SPJ protocol adapters.

use std::fs::File;
use std::path::Path;
use uj_core::verdict::Verdict;

pub mod builtin;
pub mod lemon;
pub mod testlib;

#[derive(Debug, thiserror::Error)]
pub enum CheckerError {
    #[error("IO error while reading test output or answer: {0}")]
    Io(#[from] std::io::Error),
    #[error("Checker execution failure: {0}")]
    Failure(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    pub verdict: Verdict,
    pub score_ratio: f64,
    pub message: Option<String>,
    pub diff: Option<DiffSummary>,
}

impl CheckResult {
    pub fn accepted() -> Self {
        Self {
            verdict: Verdict::Accepted,
            score_ratio: 1.0,
            message: None,
            diff: None,
        }
    }

    pub fn wrong_answer(message: impl Into<String>, diff: Option<DiffSummary>) -> Self {
        Self {
            verdict: Verdict::WrongAnswer,
            score_ratio: 0.0,
            message: Some(message.into()),
            diff,
        }
    }

    pub fn presentation_error(message: impl Into<String>, diff: Option<DiffSummary>) -> Self {
        Self {
            verdict: Verdict::PresentationError,
            score_ratio: 0.0,
            message: Some(message.into()),
            diff,
        }
    }

    pub fn partially_correct(score_ratio: f64, message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::PartiallyCorrect,
            score_ratio: score_ratio.clamp(0.0, 1.0),
            message: Some(message.into()),
            diff: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffSummary {
    pub line: usize,
    pub column: usize,
    pub token_index: usize,
    pub expected: String,
    pub actual: String,
}

/// Helper to load a file into a contiguous byte buffer using mmap for large files.
pub fn read_file_bytes(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();

    // Use memory mapping for files >= 64 KiB
    if len >= 64 * 1024 {
        // SAFETY: The file is opened read-only and mapped into memory.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(mmap.to_vec())
    } else {
        std::fs::read(path)
    }
}

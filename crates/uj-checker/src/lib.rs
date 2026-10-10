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

/// A contiguous byte buffer that is either owned on heap or memory-mapped.
pub enum FileBytes {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl AsRef<[u8]> for FileBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v.as_slice(),
            Self::Mapped(m) => &m[..],
        }
    }
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

/// Helper to load a stable file (e.g. answer file) into a contiguous byte buffer.
/// For files >= 64 KiB, maps the file into memory using Mmap and directly borrows it without `to_vec()`.
/// For smaller files or empty files, reads the bytes from the already-opened handle without re-opening.
pub fn read_file_bytes(path: &Path) -> Result<FileBytes, std::io::Error> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();

    if len == 0 {
        return Ok(FileBytes::Owned(Vec::new()));
    }

    if len >= 64 * 1024 {
        // SAFETY: The file is an answer/reference file opened read-only and is expected to remain
        // unmodified during the check comparison. The Mmap lives within FileBytes.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(FileBytes::Mapped(mmap))
    } else {
        use std::io::Read;
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf)?;
        Ok(FileBytes::Owned(buf))
    }
}

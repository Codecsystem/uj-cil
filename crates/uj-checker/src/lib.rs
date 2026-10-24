//! uj-checker: Built-in comparison algorithms and testlib/Lemon SPJ protocol adapters.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use uj_core::verdict::Verdict;

pub mod builtin;
pub mod lemon;
pub mod testlib;

/// Maximum number of characters of contestant-controlled text (tokens, lines) that is copied
/// into a [`DiffSummary`] field or a built-in checker message.
///
/// Contestant output can be arbitrarily large (e.g. a single 60 MiB line). Everything stored in a
/// [`CheckResult`] is propagated into JSON/NDJSON results, HTML reports, `last.json` and terminal
/// output, so it must be bounded.
pub const MAX_SNIPPET_CHARS: usize = 200;

/// Maximum number of characters kept from an external checker message (testlib / Lemon SPJ).
///
/// External checkers may echo contestant tokens verbatim; the limit is larger than
/// [`MAX_SNIPPET_CHARS`] because legitimate SPJ diagnostics can be a few lines long.
pub const MAX_CHECKER_MESSAGE_CHARS: usize = 1024;

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

    /// The judge itself (not the contestant) is at fault: e.g. the reference answer file is
    /// malformed for the selected checker. Mirrors testlib's `_fail` for answer-stream errors,
    /// so a broken `.ans` is reported as an author error instead of a contestant WA.
    pub fn judge_failed(message: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::JudgeFailed,
            score_ratio: 0.0,
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
    /// `true` when `expected` and/or `actual` were cut to [`MAX_SNIPPET_CHARS`] characters.
    #[serde(default)]
    pub truncated: bool,
}

impl DiffSummary {
    /// Builds a diff summary, bounding both sides to [`MAX_SNIPPET_CHARS`] characters and setting
    /// `truncated` when anything was cut.
    pub fn new(
        line: usize,
        column: usize,
        token_index: usize,
        expected: &str,
        actual: &str,
    ) -> Self {
        let (expected, t1) = clip_str(expected, MAX_SNIPPET_CHARS);
        let (actual, t2) = clip_str(actual, MAX_SNIPPET_CHARS);
        Self {
            line,
            column,
            token_index,
            expected,
            actual,
            truncated: t1 || t2,
        }
    }

    /// Like [`DiffSummary::new`] but for raw byte slices (e.g. whole output lines). Only a bounded
    /// prefix of each slice is decoded, so a multi-megabyte line is never copied.
    pub fn from_bytes(
        line: usize,
        column: usize,
        token_index: usize,
        expected: &[u8],
        actual: &[u8],
    ) -> Self {
        let (expected, t1) = clip_bytes(expected, MAX_SNIPPET_CHARS);
        let (actual, t2) = clip_bytes(actual, MAX_SNIPPET_CHARS);
        Self {
            line,
            column,
            token_index,
            expected,
            actual,
            truncated: t1 || t2,
        }
    }
}

/// Truncates `s` to at most `max_chars` characters, appending `...` when cut.
/// Returns the (possibly shortened) string and whether truncation happened.
pub fn clip_str(s: &str, max_chars: usize) -> (String, bool) {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => (format!("{}...", &s[..byte_idx]), true),
        None => (s.to_string(), false),
    }
}

/// Lossily decodes at most `max_chars` characters from `bytes` without decoding the whole slice.
/// Returns the string (with `...` appended when cut) and whether truncation happened.
pub fn clip_bytes(bytes: &[u8], max_chars: usize) -> (String, bool) {
    // Every char takes at most 4 bytes, and every invalid byte becomes one U+FFFD, so a prefix of
    // `4 * max_chars + 4` bytes always contains more than `max_chars` complete characters when the
    // slice is longer than that prefix.
    let limit = max_chars.saturating_mul(4).saturating_add(4);
    let prefix = &bytes[..bytes.len().min(limit)];
    let decoded = String::from_utf8_lossy(prefix);
    let (clipped, cut) = clip_str(&decoded, max_chars);
    if !cut && prefix.len() < bytes.len() {
        // Unreachable in practice (see above), but never claim "not truncated" when it was.
        return (format!("{clipped}..."), true);
    }
    (clipped, cut)
}

/// Strips a leading UTF-8 byte order mark (`EF BB BF`), if present.
#[inline]
pub fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
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

/// Files at or above this size are memory-mapped by [`read_file_bytes`]; smaller files are read
/// into an owned buffer.
///
/// Memory-mapping is only safe while nobody truncates or rewrites the file in place:
/// - on Linux, truncating a mapped file while it is being compared raises `SIGBUS`, which kills the
///   whole judge process (most likely in the long-lived `uj mcp` server, where a pipeline run may
///   regenerate answers that an earlier judge run still has mapped);
/// - on Windows, truncating/overwriting a mapped file fails with `ERROR_USER_MAPPED_FILE` (1224),
///   which breaks answer regeneration.
///
/// The threshold is therefore set well above the default answer-size cap (2 × the 16 MiB default
/// output limit = 32 MiB), so typical problems never map their answer files. Only problems with a
/// huge configured output limit map their answers; for those, authors must replace answer files
/// atomically (write a temp file + rename), which keeps the old inode alive on Linux.
pub const MMAP_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Helper to load a stable file (e.g. answer file) into a contiguous byte buffer.
///
/// Files of at least [`MMAP_THRESHOLD_BYTES`] are memory-mapped and borrowed without `to_vec()`;
/// see the constant's documentation for the safety trade-off. Smaller or empty files are read
/// from the already-opened handle into an owned buffer (no SIGBUS / error-1224 hazard).
pub fn read_file_bytes(path: &Path) -> Result<FileBytes, std::io::Error> {
    read_file_bytes_with_threshold(path, MMAP_THRESHOLD_BYTES)
}

fn read_file_bytes_with_threshold(
    path: &Path,
    mmap_threshold: u64,
) -> Result<FileBytes, std::io::Error> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();

    if len == 0 {
        return Ok(FileBytes::Owned(Vec::new()));
    }

    if len >= mmap_threshold {
        // SAFETY: The file is an answer/reference file opened read-only. It must not be truncated
        // or modified in place while the mapping is alive (see `MMAP_THRESHOLD_BYTES`): authoring
        // flows replace answers atomically (temp file + rename), which leaves this mapping pointing
        // at the old, unchanged inode. The Mmap lives within FileBytes.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(FileBytes::Mapped(mmap))
    } else {
        use std::io::Read;
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf)?;
        Ok(FileBytes::Owned(buf))
    }
}

/// Reserves a fresh, uniquely named file inside `dir` and returns its path.
///
/// The name combines the process id, a process-wide atomic sequence number and a timestamp, so
/// concurrent checker invocations (parallel slots, multiple `uj` processes sharing the global
/// scratch dir) never collide. The file is created with `create_new` (`O_EXCL`), so a pre-existing
/// file or symlink planted at a predictable name is never reused.
pub(crate) fn reserve_scratch_file(
    dir: &Path,
    prefix: &str,
    ext: &str,
) -> std::io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let mut last_err = None;
    for _ in 0..16 {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = dir.join(format!("{prefix}_{pid}_{seq}_{nanos:09}.{ext}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("failed to reserve scratch file")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clip_str_and_bytes() {
        let (s, t) = clip_str("abc", 5);
        assert_eq!((s.as_str(), t), ("abc", false));
        let (s, t) = clip_str("你好世界abc", 4);
        assert_eq!((s.as_str(), t), ("你好世界...", true));

        let big = vec![b'x'; 10_000_000];
        let (s, t) = clip_bytes(&big, MAX_SNIPPET_CHARS);
        assert!(t);
        assert_eq!(s.chars().count(), MAX_SNIPPET_CHARS + 3);

        // A multi-byte character cut at the decode boundary must not leak into the snippet.
        let mut mixed = vec![b'a'];
        for _ in 0..300 {
            mixed.extend_from_slice("🎉".as_bytes());
        }
        let (s, t) = clip_bytes(&mixed, 10);
        assert!(t);
        assert_eq!(s, format!("a{}...", "🎉".repeat(9)));
    }

    #[test]
    fn test_diff_summary_truncation_flag() {
        let long = "y".repeat(1000);
        let d = DiffSummary::new(1, 1, 1, "x", &long);
        assert!(d.truncated);
        assert_eq!(d.expected, "x");
        assert!(d.actual.chars().count() <= MAX_SNIPPET_CHARS + 3);

        let d = DiffSummary::new(1, 1, 1, "x", "y");
        assert!(!d.truncated);
    }

    #[test]
    fn test_strip_bom() {
        assert_eq!(strip_utf8_bom(b"\xEF\xBB\xBF3\n"), b"3\n");
        assert_eq!(strip_utf8_bom(b"3\n"), b"3\n");
    }

    #[test]
    fn test_reserve_scratch_file_unique() {
        let dir = std::env::temp_dir().join(format!("uj_checker_reserve_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let p = reserve_scratch_file(&dir, "t", "xml").unwrap();
            assert!(p.is_file());
            assert!(seen.insert(p));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_file_bytes_small_files_are_owned() {
        let dir = std::env::temp_dir().join(format!("uj_checker_rfb_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.ans");
        std::fs::write(&p, vec![b'1'; 128 * 1024]).unwrap();
        // 128 KiB answers used to be mmapped (threshold was 64 KiB); they must now be owned so the
        // file can be rewritten safely while cached.
        assert!(matches!(read_file_bytes(&p).unwrap(), FileBytes::Owned(_)));
        assert!(matches!(
            read_file_bytes_with_threshold(&p, 1024).unwrap(),
            FileBytes::Mapped(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

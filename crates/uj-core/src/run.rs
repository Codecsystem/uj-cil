use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

/// Resource constraints for sandbox execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    /// CPU time limit in milliseconds (primary basis for TLE).
    pub cpu_time_ms: u64,
    /// Wall-clock time limit in milliseconds (default: max(2*cpu, cpu+1000)).
    pub wall_time_ms: u64,
    /// Memory limit in bytes (primary basis for MLE).
    pub memory_bytes: u64,
    /// Stack size limit in bytes (None = same as memory_bytes).
    pub stack_bytes: Option<u64>,
    /// Maximum stdout output size in bytes (default: 64 MiB).
    pub output_bytes: u64,
    /// Maximum stderr tail captured in bytes (default: 64 KiB, excess truncated).
    pub stderr_bytes: u64,
    /// Maximum concurrent processes allowed (default 1 for C/C++).
    pub max_processes: u32,
}

impl Limits {
    pub const DEFAULT_OUTPUT_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB
    pub const DEFAULT_STDERR_BYTES: u64 = 64 * 1024; // 64 KiB

    pub fn new(cpu_time_ms: u64, memory_bytes: u64) -> Self {
        let wall_time_ms = std::cmp::max(cpu_time_ms * 2, cpu_time_ms + 1000);
        Self {
            cpu_time_ms,
            wall_time_ms,
            memory_bytes,
            stack_bytes: None,
            output_bytes: Self::DEFAULT_OUTPUT_BYTES,
            stderr_bytes: Self::DEFAULT_STDERR_BYTES,
            max_processes: 1,
        }
    }

    /// Effective stack limit in bytes.
    pub fn effective_stack_bytes(&self) -> u64 {
        self.stack_bytes.unwrap_or(self.memory_bytes)
    }
}

/// Standard I/O descriptor configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "path")]
pub enum Stdio {
    Null,
    File(PathBuf),
    Pipe,
    Inherit,
}

/// Platform-neutral wrapper around raw descriptor / handle (fd on Linux, Handle on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RawFdOrHandle(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SeccompProfile {
    #[default]
    Default,
    Strict,
    Compiler,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Full,
    Degraded,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Exited(i32),
    Signaled(i32),
    NtStatus(u32),
}

impl ExitStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Exited(0))
    }

    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Exited(c) => Some(*c),
            Self::Signaled(_) => None,
            Self::NtStatus(n) => Some(*n as i32),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KillReason {
    CpuLimit,
    WallLimit,
    MemoryLimit,
    OutputLimit,
    ProcessLimit,
    Seccomp,
    UserCancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CapturedOutput {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Specification of a sandboxed execution instance.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub work_dir: PathBuf,
    pub ro_binds: Vec<PathBuf>,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
    pub extra_fds: Vec<RawFdOrHandle>,
    pub limits: Limits,
    pub cpu_affinity: Option<Vec<usize>>,
    pub seccomp_profile: SeccompProfile,
    pub trusted: bool,
}

/// Result returned from the sandbox execution.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunResult {
    pub status: ExitStatus,
    pub cpu_time_us: u64,
    pub wall_time_us: u64,
    pub memory_peak_bytes: u64,
    pub killed_by: Option<KillReason>,
    pub oom_killed: bool,
    pub stdout: Option<CapturedOutput>,
    #[serde(skip)]
    pub stderr_tail: Vec<u8>,
    pub residue_cleaned: bool,
    pub sandbox_tier: Tier,
    #[serde(default)]
    pub exit_order: Option<u32>,
}

impl RunResult {
    pub fn cpu_time_ms(&self) -> u64 {
        self.cpu_time_us / 1000
    }

    pub fn wall_time_ms(&self) -> u64 {
        self.wall_time_us / 1000
    }

    pub fn memory_peak_kb(&self) -> u64 {
        self.memory_peak_bytes / 1024
    }
}

/// Result of reading a bounded artifact (e.g. output file, answer file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactReadResult {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub truncated: bool,
    pub source: String,
}

/// Errors that can occur when reading artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ReadArtifactError {
    NotFound(String),
    AccessDenied(String),
    TooLarge { size: u64, limit: u64 },
    Io(String),
    IllegalPath(String),
}

impl std::fmt::Display for ReadArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "Artifact not found: {path}"),
            Self::AccessDenied(path) => write!(f, "Permission denied reading artifact: {path}"),
            Self::TooLarge { size, limit } => {
                write!(f, "Artifact size {size} bytes exceeds limit {limit} bytes")
            }
            Self::Io(msg) => write!(f, "I/O error reading artifact: {msg}"),
            Self::IllegalPath(msg) => write!(f, "Illegal artifact path: {msg}"),
        }
    }
}

impl std::error::Error for ReadArtifactError {}

/// Safely read an artifact from `path` up to `limit_bytes`.
///
/// Distinguishes between:
/// - File not found -> `ReadArtifactError::NotFound`
/// - Permission denied -> `ReadArtifactError::AccessDenied`
/// - File size or content exceeding limit -> `ReadArtifactError::TooLarge`
/// - Legitimate 0-byte file -> `Ok(ArtifactReadResult)` with empty bytes
pub fn read_artifact_bounded(
    path: &std::path::Path,
    limit_bytes: u64,
) -> Result<ArtifactReadResult, ReadArtifactError> {
    use std::io::Read;

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadArtifactError::NotFound(path.display().to_string()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(ReadArtifactError::AccessDenied(path.display().to_string()));
        }
        Err(e) => return Err(ReadArtifactError::Io(e.to_string())),
    };

    if meta.is_dir() {
        return Err(ReadArtifactError::IllegalPath(format!(
            "Path '{}' is a directory, not a regular file",
            path.display()
        )));
    }

    let file_len = meta.len();
    if file_len > limit_bytes {
        return Err(ReadArtifactError::TooLarge {
            size: file_len,
            limit: limit_bytes,
        });
    }

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadArtifactError::NotFound(path.display().to_string()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(ReadArtifactError::AccessDenied(path.display().to_string()));
        }
        Err(e) => return Err(ReadArtifactError::Io(e.to_string())),
    };

    let mut reader = file.take(limit_bytes.saturating_add(1));
    let mut bytes = Vec::with_capacity(file_len.min(65536) as usize);
    if let Err(e) = reader.read_to_end(&mut bytes) {
        return Err(ReadArtifactError::Io(e.to_string()));
    }

    if bytes.len() as u64 > limit_bytes {
        return Err(ReadArtifactError::TooLarge {
            size: bytes.len() as u64,
            limit: limit_bytes,
        });
    }

    Ok(ArtifactReadResult {
        size: bytes.len() as u64,
        bytes,
        truncated: false,
        source: path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_artifact_bounded_success_and_empty() {
        // Normal file
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"hello world").unwrap();
        let res = read_artifact_bounded(file.path(), 100).unwrap();
        assert_eq!(res.bytes, b"hello world");
        assert_eq!(res.size, 11);

        // Empty file is a legitimate artifact, NOT an error
        let empty = NamedTempFile::new().unwrap();
        let res_empty = read_artifact_bounded(empty.path(), 100).unwrap();
        assert!(res_empty.bytes.is_empty());
        assert_eq!(res_empty.size, 0);
    }

    #[test]
    fn test_read_artifact_bounded_not_found() {
        let fake_path = std::path::Path::new("non_existent_artifact_file_12345.txt");
        let err = read_artifact_bounded(fake_path, 100).unwrap_err();
        assert!(matches!(err, ReadArtifactError::NotFound(_)));
    }

    #[test]
    fn test_read_artifact_bounded_too_large() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&[b'a'; 128]).unwrap();
        let err = read_artifact_bounded(file.path(), 64).unwrap_err();
        assert!(matches!(
            err,
            ReadArtifactError::TooLarge {
                size: 128,
                limit: 64
            }
        ));
    }
}

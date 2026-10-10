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

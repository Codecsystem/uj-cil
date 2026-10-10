//! uj-sandbox: Sandbox abstractions and platform-specific backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uj_core::run::{RunResult, RunSpec, Tier};

pub mod none;
pub mod ntstatus;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("Platform unsupported: {0}")]
    UnsupportedPlatform(String),
    #[error("Internal judge failure: {0}")]
    Internal(String),
    #[error("Environment tier mismatch: required {required:?}, detected {actual:?}")]
    TierMismatch { required: Tier, actual: Tier },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(windows)]
impl From<windows_core::Error> for SandboxError {
    fn from(err: windows_core::Error) -> Self {
        SandboxError::Internal(format!("Windows system error: {err}"))
    }
}

#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Default, Clone)]
pub struct CleanupReport {
    pub cleaned_processes: usize,
    pub cleaned_directories: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Wiring {
    pub connections: Vec<PipeConnection>,
}

#[derive(Debug, Clone)]
pub struct PipeConnection {
    pub from_proc: usize,
    pub to_proc: usize,
}

pub trait Sandbox: Send + Sync {
    fn tier(&self) -> Tier;
    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError>;
    fn run_group(
        &self,
        specs: &[RunSpec],
        wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError>;
    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError>;
}

/// Create a sandbox backend based on desired mode and host platform.
pub fn create_sandbox(
    requested_mode: &str,
    require_full_tier: bool,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    match requested_mode.to_lowercase().as_str() {
        "none" => {
            if require_full_tier {
                return Err(SandboxError::TierMismatch {
                    required: Tier::Full,
                    actual: Tier::None,
                });
            }
            Ok(Box::new(none::NoneSandbox::new()))
        }
        "full" => {
            #[cfg(windows)]
            {
                let sandbox = windows::WindowsSandbox::new(Tier::Full, false)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(target_os = "linux")]
            {
                let sandbox = linux::LinuxSandbox::new(Tier::Full, false)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                if require_full_tier {
                    return Err(SandboxError::TierMismatch {
                        required: Tier::Full,
                        actual: Tier::None,
                    });
                }
                Ok(Box::new(none::NoneSandbox::new()))
            }
        }
        "degraded" => {
            if require_full_tier {
                return Err(SandboxError::TierMismatch {
                    required: Tier::Full,
                    actual: Tier::Degraded,
                });
            }
            #[cfg(windows)]
            {
                let sandbox = windows::WindowsSandbox::new(Tier::Degraded, false)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(target_os = "linux")]
            {
                let sandbox = linux::LinuxSandbox::new(Tier::Degraded, false)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                Ok(Box::new(none::NoneSandbox::new()))
            }
        }
        "auto" => {
            #[cfg(windows)]
            {
                let sandbox = windows::WindowsSandbox::new(Tier::Full, !require_full_tier)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(target_os = "linux")]
            {
                let sandbox = linux::LinuxSandbox::new(Tier::Full, !require_full_tier)?;
                Ok(Box::new(sandbox))
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                if require_full_tier {
                    return Err(SandboxError::TierMismatch {
                        required: Tier::Full,
                        actual: Tier::None,
                    });
                }
                Ok(Box::new(none::NoneSandbox::new()))
            }
        }
        other => Err(SandboxError::Internal(format!(
            "Unknown sandbox mode: '{other}'"
        ))),
    }
}

/// Create an isolated, unique box directory inside `<base_dir>/.uj/tmp/run-<id>/box`.
/// On Windows, grants AppContainer access ONLY to uj's own AppContainer SID on this box directory.
pub fn create_isolated_box_dir(
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let cnt = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let box_dir = base_dir
        .join(".uj")
        .join("tmp")
        .join(format!("run-{ts}-{cnt}"))
        .join("box");
    std::fs::create_dir_all(&box_dir)?;

    #[cfg(windows)]
    {
        if let Ok(profile) = windows::security::AppContainerProfile::get_shared() {
            let _ = profile.grant_access_to_path(&box_dir);
        }
    }

    Ok(box_dir)
}

/// Fallback scratch directory for temp files
pub fn get_sandbox_scratch_dir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join("uj").join("tmp");
    let _ = std::fs::create_dir_all(&p);
    p
}

#[cfg(windows)]
pub use windows::process::{detect_p_core_primary_cpus, prewarm_binary};

#[cfg(not(windows))]
pub fn prewarm_binary(_exe_path: &std::path::Path) {}

#[cfg(not(windows))]
pub fn detect_p_core_primary_cpus() -> Vec<usize> {
    (0..num_cpus::get()).collect()
}

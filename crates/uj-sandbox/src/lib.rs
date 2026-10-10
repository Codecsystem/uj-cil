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

/// Returns a scratch/temporary directory suitable for sandboxed runs.
/// On Windows with AppContainer, ensures permissions (ALL APPLICATION PACKAGES) are granted.
pub fn get_sandbox_scratch_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let p = std::path::PathBuf::from(r"C:\ProgramData\uj\tmp");
        if std::fs::create_dir_all(&p).is_ok() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                let _ = windows::security::AppContainerProfile::grant_all_application_packages(&p);
            });
            p
        } else {
            let fallback = std::env::temp_dir().join("uj_tmp");
            let _ = std::fs::create_dir_all(&fallback);
            let _ =
                windows::security::AppContainerProfile::grant_all_application_packages(&fallback);
            fallback
        }
    }
    #[cfg(not(windows))]
    {
        let p = std::env::temp_dir().join("uj_tmp");
        let _ = std::fs::create_dir_all(&p);
        p
    }
}

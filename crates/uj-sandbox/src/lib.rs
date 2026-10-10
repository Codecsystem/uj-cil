//! uj-sandbox: Sandbox abstractions and platform-specific backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uj_core::run::{RunResult, RunSpec, Tier};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("Platform unsupported: {0}")]
    UnsupportedPlatform(String),
    #[error("Internal judge failure: {0}")]
    Internal(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub cleaned_processes: usize,
    pub cleaned_directories: usize,
}

pub trait Sandbox: Send + Sync {
    fn tier(&self) -> Tier;
    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError>;
    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError>;
}

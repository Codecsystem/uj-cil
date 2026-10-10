//! Windows Sandbox implementation utilizing Job Objects, IOCP, and AppContainer.

pub mod job;
pub mod process;
pub mod security;

use uj_core::run::{RunResult, RunSpec, Tier};

use crate::windows::job::JobObject;
use crate::windows::security::AppContainerProfile;
use crate::{CancelToken, CleanupReport, Sandbox, SandboxError, Wiring};
use std::sync::Arc;

pub struct WindowsSandbox {
    tier: Tier,
    app_container: Option<Arc<AppContainerProfile>>,
}

impl WindowsSandbox {
    pub fn new(requested_tier: Tier, allow_fallback: bool) -> Result<Self, SandboxError> {
        match requested_tier {
            Tier::None => Err(SandboxError::Internal(
                "Use NoneSandbox for Tier::None".to_string(),
            )),
            Tier::Degraded => Ok(Self {
                tier: Tier::Degraded,
                app_container: None,
            }),
            Tier::Full => match AppContainerProfile::create_isolated() {
                Ok(profile) => Ok(Self {
                    tier: Tier::Full,
                    app_container: Some(Arc::new(profile)),
                }),
                Err(err) => {
                    if !allow_fallback {
                        return Err(SandboxError::TierMismatch {
                            required: Tier::Full,
                            actual: Tier::Degraded,
                        });
                    }
                    tracing::warn!(
                        "AppContainer profile unavailable ({err}); falling back to Degraded tier (Job Object only)"
                    );
                    Ok(Self {
                        tier: Tier::Degraded,
                        app_container: None,
                    })
                }
            },
        }
    }
}

impl Sandbox for WindowsSandbox {
    fn tier(&self) -> Tier {
        self.tier
    }

    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError> {
        // 1. Create a dedicated Job Object for this single run
        let job = JobObject::create(None)
            .map_err(|e| SandboxError::Internal(format!("Failed to create Job Object: {e}")))?;

        // 2. Set memory, process limit, and core affinity
        job.set_limits(
            spec.limits.memory_bytes,
            spec.limits.max_processes,
            spec.cpu_affinity.as_deref(),
        )
        .map_err(|e| SandboxError::Internal(format!("Failed to set Job limits: {e}")))?;

        if self.tier == Tier::Degraded {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "WARNING: Running in Degraded sandbox tier (Job Object only). Network and filesystem isolation are disabled."
                );
            }
        }

        // 3. Execute process with strict handle inheritance
        let ac = if spec.trusted {
            None
        } else {
            self.app_container.as_deref()
        };
        process::execute_in_job(spec, &job, ac, cancel, self.tier)
    }

    fn run_group(
        &self,
        specs: &[RunSpec],
        wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError> {
        let ac = self.app_container.as_deref();
        process::execute_group_in_jobs(specs, wiring, ac, cancel, self.tier)
    }

    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError> {
        // Windows Job Objects automatically terminate with KILL_ON_JOB_CLOSE when process dies.
        Ok(CleanupReport::default())
    }
}

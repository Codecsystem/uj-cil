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
    slot_id: usize,
    authorized_slot_dirs: parking_lot::Mutex<std::collections::HashSet<std::path::PathBuf>>,
}

static WINDOWS_SLOT_COUNTER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(1);

impl WindowsSandbox {
    pub fn new(requested_tier: Tier, allow_fallback: bool) -> Result<Self, SandboxError> {
        let slot_id = WINDOWS_SLOT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match requested_tier {
            Tier::None => Err(SandboxError::Internal(
                "Use NoneSandbox for Tier::None".to_string(),
            )),
            Tier::Degraded => Ok(Self {
                tier: Tier::Degraded,
                app_container: None,
                slot_id,
                authorized_slot_dirs: parking_lot::Mutex::new(std::collections::HashSet::new()),
            }),
            Tier::Full => match AppContainerProfile::create_isolated_for_slot(slot_id) {
                Ok(profile) => Ok(Self {
                    tier: Tier::Full,
                    app_container: Some(Arc::new(profile)),
                    slot_id,
                    authorized_slot_dirs: parking_lot::Mutex::new(std::collections::HashSet::new()),
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
                        slot_id,
                        authorized_slot_dirs: parking_lot::Mutex::new(std::collections::HashSet::new()),
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

    fn create_box_dir(&self, base_dir: &std::path::Path) -> Result<std::path::PathBuf, SandboxError> {
        let runs_base = base_dir.join(".uj").join("tmp").join("runs");
        let slot_dir = runs_base.join(format!("slot-{}", self.slot_id));
        std::fs::create_dir_all(&slot_dir)?;

        if let Some(ref ac) = self.app_container {
            let mut set = self.authorized_slot_dirs.lock();
            if set.insert(slot_dir.clone()) {
                let _ = ac.grant_access_to_path(&slot_dir);
            }
        }

        use std::time::SystemTime;
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let cnt = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let box_dir = slot_dir
            .join(format!("run-{ts}-{cnt}"))
            .join("box");
        std::fs::create_dir_all(&box_dir)?;

        Ok(box_dir)
    }

    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError> {
        // Ensure this slot's AppContainer profile has access to the work_dir
        if let Some(ref ac) = self.app_container {
            let mut set = self.authorized_slot_dirs.lock();
            if set.insert(spec.work_dir.clone()) {
                let _ = ac.grant_access_to_path(&spec.work_dir);
            }
        }

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
        let cleaned = security::cleanup_stale_appcontainers();
        Ok(CleanupReport {
            cleaned_processes: cleaned,
            cleaned_directories: 0,
        })
    }
}

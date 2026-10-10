//! Linux Sandbox implementation utilizing userns, pidns, mountns, cgroup v2, and seccomp.

pub mod cgroup;
pub mod runner;
pub mod seccomp;

use self::cgroup::CgroupV2;
use crate::{CancelToken, CleanupReport, Sandbox, SandboxError, Wiring};
use std::path::PathBuf;
use uj_core::run::{RunResult, RunSpec, Tier};

pub struct LinuxSandbox {
    tier: Tier,
    cgroup_base: Option<PathBuf>,
}

impl LinuxSandbox {
    pub fn new(requested_tier: Tier, allow_fallback: bool) -> Result<Self, SandboxError> {
        // Verify kernel version >= 5.19
        let uname = rustix::system::uname();
        let release = uname.release().to_string_lossy();
        let parts: Vec<&str> = release.split('.').collect();
        if parts.len() >= 2 {
            let major: u32 = parts[0].parse().unwrap_or(0);
            let minor: u32 = parts[1]
                .split('-')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            if (major < 5 || (major == 5 && minor < 19))
                && !allow_fallback
                && requested_tier == Tier::Full
            {
                return Err(SandboxError::Internal(format!(
                    "Kernel version {release} is below required minimum 5.19"
                )));
            }
        }

        match requested_tier {
            Tier::None => Ok(Self {
                tier: Tier::None,
                cgroup_base: None,
            }),
            Tier::Degraded => {
                let base = CgroupV2::find_base_dir().ok();
                Ok(Self {
                    tier: Tier::Degraded,
                    cgroup_base: base,
                })
            }
            Tier::Full => match CgroupV2::find_base_dir() {
                Ok(base) => Ok(Self {
                    tier: Tier::Full,
                    cgroup_base: Some(base),
                }),
                Err(e) => {
                    if !allow_fallback {
                        return Err(SandboxError::TierMismatch {
                            required: Tier::Full,
                            actual: Tier::Degraded,
                        });
                    }
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!(
                            "WARNING: Linux Full sandbox unavailable ({e}); falling back to Degraded tier (namespaces + seccomp + rlimit, without cgroup v2)."
                        );
                    }
                    Ok(Self {
                        tier: Tier::Degraded,
                        cgroup_base: None,
                    })
                }
            },
        }
    }
}

impl Sandbox for LinuxSandbox {
    fn tier(&self) -> Tier {
        self.tier
    }

    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError> {
        if self.tier == Tier::None {
            let fallback = crate::none::NoneSandbox::new();
            let mut res = fallback.run(spec, cancel)?;
            res.sandbox_tier = self.tier;
            Ok(res)
        } else {
            runner::execute_in_sandbox(spec, self.cgroup_base.as_deref(), cancel, self.tier)
        }
    }

    fn run_group(
        &self,
        specs: &[RunSpec],
        wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError> {
        if self.tier == Tier::None {
            let fallback = crate::none::NoneSandbox::new();
            let mut res = fallback.run_group(specs, wiring, cancel)?;
            for r in &mut res {
                r.sandbox_tier = self.tier;
            }
            Ok(res)
        } else {
            runner::execute_group_in_sandbox(
                specs,
                wiring,
                self.cgroup_base.as_deref(),
                cancel,
                self.tier,
            )
        }
    }

    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError> {
        if let Some(ref base) = self.cgroup_base {
            let _ = std::fs::remove_dir_all(base);
        }
        Ok(CleanupReport::default())
    }
}

impl Drop for LinuxSandbox {
    fn drop(&mut self) {
        if let Some(ref base) = self.cgroup_base {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

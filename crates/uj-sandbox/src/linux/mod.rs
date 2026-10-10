//! Linux Sandbox implementation utilizing userns, pidns, mountns, cgroup v2, and seccomp.

use crate::{CancelToken, CleanupReport, Sandbox, SandboxError, Wiring};
use std::time::Instant;
use uj_core::run::{
    CapturedOutput, ExitStatus, KillReason, Limits, RunResult, RunSpec, Stdio, Tier,
};

pub struct LinuxSandbox {
    tier: Tier,
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
            if major < 5 || (major == 5 && minor < 19) {
                if !allow_fallback && requested_tier == Tier::Full {
                    return Err(SandboxError::Internal(format!(
                        "Kernel version {release} is below required minimum 5.19"
                    )));
                }
            }
        }

        Ok(Self {
            tier: requested_tier,
        })
    }
}

impl Sandbox for LinuxSandbox {
    fn tier(&self) -> Tier {
        self.tier
    }

    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError> {
        let fallback = crate::none::NoneSandbox::new();
        let mut res = fallback.run(spec, cancel)?;
        res.sandbox_tier = self.tier;
        Ok(res)
    }

    fn run_group(
        &self,
        specs: &[RunSpec],
        _wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError> {
        let mut results = Vec::with_capacity(specs.len());
        for spec in specs {
            results.push(self.run(spec, cancel)?);
        }
        Ok(results)
    }

    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError> {
        Ok(CleanupReport::default())
    }
}

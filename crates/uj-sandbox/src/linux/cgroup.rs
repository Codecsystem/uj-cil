//! Linux cgroup v2 resource management.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use uj_core::run::Limits;
use crate::SandboxError;

static COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct CgroupV2 {
    path: PathBuf,
}

fn enable_subtree_controllers(dir: &Path) {
    let controllers_file = dir.join("cgroup.controllers");
    let Ok(available) = fs::read_to_string(&controllers_file) else {
        return;
    };
    let mut to_enable = Vec::new();
    for c in ["cpu", "memory", "pids"] {
        if available.split_whitespace().any(|s| s == c) {
            to_enable.push(format!("+{c}"));
        }
    }
    if to_enable.is_empty() {
        return;
    }
    let control_str = to_enable.join(" ");

    // In cgroup v2, a non-root cgroup can only write to cgroup.subtree_control
    // if cgroup.procs is empty (no internal processes rule).
    // If there are processes in dir, move them to dir/runner first.
    let procs_file = dir.join("cgroup.procs");
    if let Ok(procs) = fs::read_to_string(&procs_file) {
        let pids: Vec<&str> = procs
            .lines()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !pids.is_empty() {
            let runner_dir = dir.join("runner");
            let _ = fs::create_dir_all(&runner_dir);
            let runner_procs = runner_dir.join("cgroup.procs");
            for pid in pids {
                let _ = fs::write(&runner_procs, pid);
            }
        }
    }
    let _ = fs::write(dir.join("cgroup.subtree_control"), &control_str);
}

static CACHED_BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

impl CgroupV2 {
    /// Detects the writable base cgroup v2 directory (supports systemd user delegation or root).
    pub fn find_base_dir() -> Result<PathBuf, SandboxError> {
        if let Some(path) = CACHED_BASE_DIR.get() {
            return Ok(path.clone());
        }
        let base = Self::find_base_dir_uncached()?;
        let _ = CACHED_BASE_DIR.set(base.clone());
        Ok(base)
    }

    fn find_base_dir_uncached() -> Result<PathBuf, SandboxError> {
        let cgroup_mount = Path::new("/sys/fs/cgroup");
        if !cgroup_mount.join("cgroup.controllers").exists() {
            return Err(SandboxError::Internal(
                "cgroup v2 is not mounted at /sys/fs/cgroup".to_string(),
            ));
        }

        // 1. Check current process's cgroup from /proc/self/cgroup
        if let Ok(content) = fs::read_to_string("/proc/self/cgroup") {
            for line in content.lines() {
                // v2 entry starts with "0::"
                if let Some(rel) = line.strip_prefix("0::") {
                    let rel_clean = rel.trim_start_matches('/');
                    let mut candidate = cgroup_mount.join(rel_clean);
                    // If candidate is inside runner or uj, ascend to parent scope
                    while candidate.file_name() == Some(std::ffi::OsStr::new("runner"))
                        || candidate.file_name() == Some(std::ffi::OsStr::new("uj"))
                    {
                        if let Some(parent) = candidate.parent() {
                            candidate = parent.to_path_buf();
                        } else {
                            break;
                        }
                    }
                    while candidate.starts_with(cgroup_mount) {
                        if candidate.is_dir() {
                            // Check if candidate is writable
                            let test_dir = candidate.join(format!(".uj_test_{}", std::process::id()));
                            if fs::create_dir(&test_dir).is_ok() {
                                let _ = fs::remove_dir(&test_dir);
                                enable_subtree_controllers(&candidate);
                                let uj_base = candidate.join("uj");
                                let _ = fs::create_dir_all(&uj_base);
                                enable_subtree_controllers(&uj_base);
                                return Ok(uj_base);
                            }
                        }
                        if let Some(parent) = candidate.parent() {
                            candidate = parent.to_path_buf();
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        // 2. Check standard systemd user delegation path: /sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/app.slice
        let uid = unsafe { libc::getuid() };
        let user_slice_app = cgroup_mount
            .join("user.slice")
            .join(format!("user-{uid}.slice"))
            .join(format!("user@{uid}.service"))
            .join("app.slice");

        if user_slice_app.is_dir() {
            enable_subtree_controllers(&user_slice_app);
            let uj_base = user_slice_app.join("uj");
            let _ = fs::create_dir_all(&uj_base);
            enable_subtree_controllers(&uj_base);
            let test_dir = uj_base.join(format!(".uj_test_{}", std::process::id()));
            if fs::create_dir(&test_dir).is_ok() {
                let _ = fs::remove_dir(&test_dir);
                return Ok(uj_base);
            }
        }

        // 3. Fallback to /sys/fs/cgroup/uj (if running as root)
        let root_uj = cgroup_mount.join("uj");
        let _ = fs::create_dir_all(&root_uj);
        enable_subtree_controllers(cgroup_mount);
        enable_subtree_controllers(&root_uj);
        let test_dir = root_uj.join(format!(".uj_test_{}", std::process::id()));
        if fs::create_dir(&test_dir).is_ok() {
            let _ = fs::remove_dir(&test_dir);
            return Ok(root_uj);
        }

        Err(SandboxError::Internal(
            "No writable cgroup v2 directory found. Ensure systemd user delegation or root access is available.".to_string(),
        ))
    }

    /// Create a dedicated sub-cgroup for a single test run.
    pub fn create_for_run(base_dir: &Path, prefix: &str) -> Result<Self, SandboxError> {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
        let sub_path = base_dir.join(format!("{prefix}-{ts}-{cnt}"));

        fs::create_dir(&sub_path).map_err(|e| {
            SandboxError::Internal(format!("Failed to create cgroup directory {}: {e}", sub_path.display()))
        })?;

        Ok(Self { path: sub_path })
    }

    pub fn set_limits(
        &self,
        limits: &Limits,
        cpu_affinity: Option<&[usize]>,
    ) -> Result<(), SandboxError> {
        // 1. memory.max
        if limits.memory_bytes > 0 {
            let mem_file = self.path.join("memory.max");
            fs::write(&mem_file, limits.memory_bytes.to_string()).map_err(|e| {
                SandboxError::Internal(format!("Failed to set memory.max in {}: {e}", self.path.display()))
            })?;
            // Enforce hard memory limit by forbidding swap
            let swap_file = self.path.join("memory.swap.max");
            let _ = fs::write(swap_file, "0");
            let oom_group_file = self.path.join("memory.oom.group");
            let _ = fs::write(oom_group_file, "1");
        }

        // 2. pids.max
        if limits.max_processes > 0 {
            let pids_file = self.path.join("pids.max");
            // Allow max_processes + 3 for mini-init and helper
            let max_pids = limits.max_processes + 3;
            let _ = fs::write(&pids_file, max_pids.to_string());
        }

        // 3. cpuset.cpus (affinity)
        if let Some(cpus) = cpu_affinity
            && !cpus.is_empty()
        {
            let mask = cpus
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = fs::write(self.path.join("cpuset.cpus"), mask);
        }

        Ok(())
    }

    /// Add a process PID to this cgroup.
    pub fn add_process(&self, pid: i32) -> Result<(), SandboxError> {
        let procs_file = self.path.join("cgroup.procs");
        fs::write(&procs_file, pid.to_string()).map_err(|e| {
            SandboxError::Internal(format!("Failed to add pid {pid} to cgroup {}: {e}", self.path.display()))
        })
    }

    /// Read peak memory consumed by the cgroup in bytes.
    pub fn read_peak_memory(&self) -> u64 {
        let peak_file = self.path.join("memory.peak");
        fs::read_to_string(peak_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Read total CPU time consumed by the cgroup in microseconds.
    pub fn read_cpu_time_us(&self) -> u64 {
        let stat_file = self.path.join("cpu.stat");
        if let Ok(content) = fs::read_to_string(stat_file) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("usage_usec ") {
                    return rest.trim().parse::<u64>().unwrap_or(0);
                }
            }
        }
        0
    }

    /// Check if the cgroup triggered an out-of-memory event.
    pub fn read_oom_killed(&self) -> bool {
        let events_file = self.path.join("memory.events");
        if let Ok(content) = fs::read_to_string(events_file) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("oom_kill ").or_else(|| line.strip_prefix("oom "))
                    && rest.trim().parse::<u64>().is_ok_and(|count| count > 0)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Atomically terminate all processes in this cgroup.
    pub fn kill(&self) {
        let kill_file = self.path.join("cgroup.kill");
        let _ = fs::write(kill_file, "1");
    }

    /// Clean up and remove this cgroup directory.
    pub fn cleanup(&self) {
        self.kill();
        // Give kernel a brief moment to reap processes, then rmdir
        for _ in 0..10 {
            if fs::remove_dir(&self.path).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CgroupV2 {
    fn drop(&mut self) {
        self.cleanup();
    }
}

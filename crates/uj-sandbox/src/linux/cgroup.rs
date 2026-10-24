//! Linux cgroup v2 resource management.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::SystemTime;

use crate::{CleanupReport, SandboxError};
use uj_core::run::Limits;

static COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct CgroupV2 {
    path: PathBuf,
    cpu_stat_file: parking_lot::Mutex<Option<fs::File>>,
    mem_events_file: parking_lot::Mutex<Option<fs::File>>,
}

/// Reference-counted handle to the per-process cgroup base directory (`.../uj/proc_<pid>`).
/// When the last `LinuxSandbox` instance in this process drops, the `proc_<pid>` cgroup
/// directory tree is removed bottom-up via `rmdir` (never `remove_dir_all`, which fails with
/// `EPERM` on cgroupfs control files).
#[derive(Debug)]
pub struct CgroupBase {
    path: PathBuf,
}

impl CgroupBase {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CgroupBase {
    fn drop(&mut self) {
        let mut guard = CACHED_BASE.lock();
        if guard.as_ref().and_then(|w| w.upgrade()).is_none() {
            *guard = None;
            let _ = remove_cgroup_tree(&self.path);
        }
    }
}

fn host_has_active_swap() -> bool {
    static HAS_SWAP: OnceLock<bool> = OnceLock::new();
    *HAS_SWAP.get_or_init(|| {
        fs::read_to_string("/proc/swaps")
            .map(|s| s.lines().skip(1).any(|line| !line.trim().is_empty()))
            .unwrap_or(false)
    })
}

fn enable_subtree_controllers(dir: &Path) {
    let controllers_file = dir.join("cgroup.controllers");
    let Ok(available) = fs::read_to_string(&controllers_file) else {
        return;
    };
    let mut to_enable = Vec::new();
    for c in ["cpu", "cpuset", "memory", "pids"] {
        if available.split_whitespace().any(|s| s == c) {
            to_enable.push(format!("+{c}"));
        }
    }
    if to_enable.is_empty() {
        return;
    }

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

    let subtree_file = dir.join("cgroup.subtree_control");
    let control_str = to_enable.join(" ");
    if fs::write(&subtree_file, &control_str).is_err() {
        for item in &to_enable {
            let _ = fs::write(&subtree_file, item);
        }
    }
}

/// Recursively kills processes and removes a cgroup directory tree bottom-up using `fs::remove_dir`.
pub fn remove_cgroup_tree(dir: &Path) -> bool {
    if !dir.exists() {
        return true;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let _ = remove_cgroup_tree(&entry.path());
            }
        }
    }
    let kill_file = dir.join("cgroup.kill");
    let _ = fs::write(&kill_file, "1");
    let procs_file = dir.join("cgroup.procs");
    if let Ok(content) = fs::read_to_string(&procs_file) {
        for line in content.lines() {
            if let Ok(pid) = line.trim().parse::<i32>()
                && pid > 0
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    for _ in 0..10 {
        if fs::remove_dir(dir).is_ok() || !dir.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if Path::new(&format!("/proc/{pid}")).exists() {
        return true;
    }
    let ret = unsafe { libc::kill(pid, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes stale `proc_<dead_pid>` cgroup directories and leftover `.uj_rootfs_<dead_pid>_*` directories.
pub fn cleanup_stale_resources(base_opt: Option<&Path>) -> CleanupReport {
    let mut report = CleanupReport::default();
    let self_pid = std::process::id() as i32;

    let mut uj_parents = Vec::new();
    if let Some(base) = base_opt
        && let Some(parent) = base.parent()
    {
        uj_parents.push(parent.to_path_buf());
    }
    if let Ok(content) = fs::read_to_string("/proc/self/cgroup") {
        let cgroup_mount = Path::new("/sys/fs/cgroup");
        for line in content.lines() {
            if let Some(rel) = line.strip_prefix("0::") {
                let mut candidate = cgroup_mount.join(rel.trim_start_matches('/'));
                while candidate.file_name() == Some(std::ffi::OsStr::new("runner"))
                    || candidate.file_name() == Some(std::ffi::OsStr::new("uj"))
                    || candidate
                        .file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with("proc_"))
                {
                    if let Some(p) = candidate.parent() {
                        candidate = p.to_path_buf();
                    } else {
                        break;
                    }
                }
                let uj_dir = candidate.join("uj");
                if uj_dir.is_dir() && !uj_parents.contains(&uj_dir) {
                    uj_parents.push(uj_dir);
                }
            }
        }
    }

    for uj_parent in uj_parents {
        if let Ok(entries) = fs::read_dir(&uj_parent) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if let Some(pid_str) = name_str.strip_prefix("proc_")
                    && let Ok(pid) = pid_str.parse::<i32>()
                    && pid != self_pid
                    && !is_pid_alive(pid)
                    && remove_cgroup_tree(&entry.path())
                {
                    report.cleaned_directories += 1;
                }
            }
        }
    }

    // Also clean up any leftover `.uj_rootfs_<dead_pid>_*` directories in /dev/shm, /run, /tmp
    let self_uid = unsafe { libc::getuid() };
    for root in [Path::new("/dev/shm"), Path::new("/run"), Path::new("/tmp")] {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(rest) = name_str.strip_prefix(".uj_rootfs_") else {
                continue;
            };
            let Some(pid_str) = rest.split('_').next() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<i32>() else {
                continue;
            };
            if pid == self_pid || is_pid_alive(pid) {
                continue;
            }
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = entry.path().symlink_metadata()
                && meta.uid() == self_uid
                && fs::remove_dir_all(entry.path()).is_ok()
            {
                report.cleaned_directories += 1;
            }
        }
    }

    report
}

static CACHED_BASE: parking_lot::Mutex<Option<Weak<CgroupBase>>> = parking_lot::Mutex::new(None);

fn verify_required_controllers(test_dir: &Path) -> Result<(), SandboxError> {
    for required in ["cpu.stat", "memory.max", "pids.max"] {
        if !test_dir.join(required).exists() {
            return Err(SandboxError::Internal(format!(
                "Required cgroup v2 controller file '{required}' is missing in {}",
                test_dir.display()
            )));
        }
    }
    if host_has_active_swap() {
        let swap_max = test_dir.join("memory.swap.max");
        if !swap_max.exists() || fs::write(&swap_max, "0").is_err() {
            return Err(SandboxError::Internal(format!(
                "Host has active swap but 'memory.swap.max' is not writable in {}",
                test_dir.display()
            )));
        }
    }
    Ok(())
}

impl CgroupV2 {
    /// Returns a reference-counted handle to the process's cgroup v2 base directory.
    pub fn find_base() -> Result<Arc<CgroupBase>, SandboxError> {
        let mut guard = CACHED_BASE.lock();
        if let Some(ref weak) = *guard
            && let Some(arc) = weak.upgrade()
            && arc.path.is_dir()
        {
            return Ok(arc);
        }
        let path = Self::find_base_dir_uncached()?;
        let arc = Arc::new(CgroupBase { path });
        *guard = Some(Arc::downgrade(&arc));
        Ok(arc)
    }

    /// Detects the writable base cgroup v2 directory (supports systemd user delegation or root).
    pub fn find_base_dir() -> Result<PathBuf, SandboxError> {
        Ok(Self::find_base()?.path().to_path_buf())
    }

    fn find_base_dir_uncached() -> Result<PathBuf, SandboxError> {
        let cgroup_mount = Path::new("/sys/fs/cgroup");
        if !cgroup_mount.join("cgroup.controllers").exists() {
            return Err(SandboxError::Internal(
                "cgroup v2 is not mounted at /sys/fs/cgroup".to_string(),
            ));
        }

        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
        let test_name = format!(
            ".uj_test_{}_{}_{cnt}_{ts}",
            std::process::id(),
            rustix::thread::gettid().as_raw_nonzero()
        );

        // 1. Check current process's cgroup from /proc/self/cgroup
        if let Ok(content) = fs::read_to_string("/proc/self/cgroup") {
            for line in content.lines() {
                // v2 entry starts with "0::"
                if let Some(rel) = line.strip_prefix("0::") {
                    let rel_clean = rel.trim_start_matches('/');
                    let mut candidate = cgroup_mount.join(rel_clean);
                    // If candidate is inside runner, uj, or proc_<pid>, ascend to its scope
                    while candidate.file_name() == Some(std::ffi::OsStr::new("runner"))
                        || candidate.file_name() == Some(std::ffi::OsStr::new("uj"))
                        || candidate
                            .file_name()
                            .and_then(|s| s.to_str())
                            .map(|s| s.starts_with("proc_"))
                            .unwrap_or(false)
                    {
                        if let Some(parent) = candidate.parent() {
                            candidate = parent.to_path_buf();
                        } else {
                            break;
                        }
                    }
                    if candidate.is_dir() {
                        let probe_dir = candidate.join(&test_name);
                        if fs::create_dir(&probe_dir).is_ok() {
                            let _ = fs::remove_dir(&probe_dir);
                            enable_subtree_controllers(&candidate);
                            let pid = std::process::id();
                            let uj_parent = candidate.join("uj");
                            let _ = fs::create_dir_all(&uj_parent);
                            enable_subtree_controllers(&uj_parent);
                            let uj_base = uj_parent.join(format!("proc_{pid}"));
                            let _ = fs::create_dir_all(&uj_base);
                            enable_subtree_controllers(&uj_base);

                            let test_dir = uj_base.join(&test_name);
                            if fs::create_dir(&test_dir).is_ok() {
                                let check_res = verify_required_controllers(&test_dir);
                                let _ = fs::remove_dir(&test_dir);
                                check_res?;
                                return Ok(uj_base);
                            }
                        }
                    }
                }
            }
        }

        // 2. Fallback to /sys/fs/cgroup/uj/proc_<pid> (if running as root)
        if unsafe { libc::getuid() } == 0 {
            let pid = std::process::id();
            let root_uj_parent = cgroup_mount.join("uj");
            let _ = fs::create_dir_all(&root_uj_parent);
            enable_subtree_controllers(cgroup_mount);
            enable_subtree_controllers(&root_uj_parent);
            let root_uj = root_uj_parent.join(format!("proc_{pid}"));
            let _ = fs::create_dir_all(&root_uj);
            enable_subtree_controllers(&root_uj);
            let test_dir = root_uj.join(&test_name);
            if fs::create_dir(&test_dir).is_ok() {
                let check_res = verify_required_controllers(&test_dir);
                let _ = fs::remove_dir(&test_dir);
                check_res?;
                return Ok(root_uj);
            }
        }

        Err(SandboxError::Internal(
            "No writable cgroup v2 directory found in current scope. Ensure running within a systemd delegated scope (e.g. systemd-run --user --scope -p Delegate=yes) or as root.".to_string(),
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
            SandboxError::Internal(format!(
                "Failed to create cgroup directory {}: {e}",
                sub_path.display()
            ))
        })?;

        Ok(Self {
            path: sub_path,
            cpu_stat_file: parking_lot::Mutex::new(None),
            mem_events_file: parking_lot::Mutex::new(None),
        })
    }

    pub fn set_limits(
        &self,
        limits: &Limits,
        cpu_affinity: Option<&[usize]>,
    ) -> Result<(), SandboxError> {
        // 1. memory.max & memory.swap.max
        if limits.memory_bytes > 0 {
            let mem_file = self.path.join("memory.max");
            fs::write(&mem_file, limits.memory_bytes.to_string()).map_err(|e| {
                SandboxError::Internal(format!(
                    "Failed to set memory.max in {}: {e}",
                    self.path.display()
                ))
            })?;
            // Enforce hard memory limit by forbidding swap when swap exists on host
            let swap_file = self.path.join("memory.swap.max");
            if swap_file.exists() || host_has_active_swap() {
                fs::write(&swap_file, "0").map_err(|e| {
                    SandboxError::Internal(format!(
                        "Failed to set memory.swap.max in {}: {e}",
                        self.path.display()
                    ))
                })?;
            }
            let oom_group_file = self.path.join("memory.oom.group");
            if oom_group_file.exists() {
                let _ = fs::write(oom_group_file, "1");
            }
        }

        // 2. pids.max
        if limits.max_processes > 0 {
            let pids_file = self.path.join("pids.max");
            // Allow max_processes + 3 for mini-init and helper
            let max_pids = limits.max_processes + 3;
            fs::write(&pids_file, max_pids.to_string()).map_err(|e| {
                SandboxError::Internal(format!(
                    "Failed to set pids.max in {}: {e}",
                    self.path.display()
                ))
            })?;
        }

        // 3. cpuset.cpus (affinity)
        if let Some(cpus) = cpu_affinity
            && !cpus.is_empty()
        {
            let cpuset_file = self.path.join("cpuset.cpus");
            if cpuset_file.exists() {
                let mask = cpus
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = fs::write(cpuset_file, mask);
            }
        }

        Ok(())
    }

    /// Add a process PID to this cgroup.
    pub fn add_process(&self, pid: i32) -> Result<(), SandboxError> {
        let procs_file = self.path.join("cgroup.procs");
        fs::write(&procs_file, pid.to_string()).map_err(|e| {
            SandboxError::Internal(format!(
                "Failed to add pid {pid} to cgroup {}: {e}",
                self.path.display()
            ))
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
        use std::os::unix::fs::FileExt;
        let mut guard = self.cpu_stat_file.lock();
        if guard.is_none() {
            *guard = fs::File::open(self.path.join("cpu.stat")).ok();
        }
        if let Some(ref f) = *guard {
            let mut buf = [0u8; 512];
            if let Ok(n) = f.read_at(&mut buf, 0)
                && let Ok(content) = std::str::from_utf8(&buf[..n])
            {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("usage_usec ") {
                        return rest.trim().parse::<u64>().unwrap_or(0);
                    }
                }
            }
        }
        0
    }

    /// Check if the cgroup triggered an out-of-memory event.
    pub fn read_oom_killed(&self) -> bool {
        use std::os::unix::fs::FileExt;
        let mut guard = self.mem_events_file.lock();
        if guard.is_none() {
            *guard = fs::File::open(self.path.join("memory.events")).ok();
        }
        if let Some(ref f) = *guard {
            let mut buf = [0u8; 512];
            if let Ok(n) = f.read_at(&mut buf, 0)
                && let Ok(content) = std::str::from_utf8(&buf[..n])
            {
                for line in content.lines() {
                    if let Some(rest) = line
                        .strip_prefix("oom_kill ")
                        .or_else(|| line.strip_prefix("oom "))
                        && rest.trim().parse::<u64>().is_ok_and(|count| count > 0)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Atomically terminate all processes in this cgroup.
    pub fn kill(&self) {
        let kill_file = self.path.join("cgroup.kill");
        let _ = fs::write(kill_file, "1");

        let procs_file = self.path.join("cgroup.procs");
        if let Ok(content) = fs::read_to_string(&procs_file) {
            for line in content.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }

    /// Clean up and remove this cgroup directory.
    pub fn cleanup(&self) {
        let _ = self.cpu_stat_file.lock().take();
        let _ = self.mem_events_file.lock().take();
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

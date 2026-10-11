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
    fn create_box_dir(
        &self,
        base_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf, SandboxError> {
        create_isolated_box_dir(base_dir).map_err(SandboxError::Io)
    }
    fn authorize_executable(&self, _path: &std::path::Path) -> Result<(), SandboxError> {
        Ok(())
    }
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

#[cfg(windows)]
static AUTHORIZED_RUNS_DIRS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

/// Create an isolated, unique box directory inside `<base_dir>/.uj/tmp/runs/run-<id>/box`.
/// Authorizes `<base_dir>/.uj/tmp/runs` once per base_dir so child box directories automatically
/// inherit full access without incurring per-test `icacls` process spawn overhead.
pub fn create_isolated_box_dir(
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let runs_dir = base_dir.join(".uj").join("tmp").join("runs");
    std::fs::create_dir_all(&runs_dir)?;

    #[cfg(windows)]
    {
        let needs_auth = {
            let mut set = AUTHORIZED_RUNS_DIRS.lock();
            set.insert(runs_dir.clone())
        };
        if needs_auth && let Ok(profile) = windows::security::AppContainerProfile::get_shared() {
            let _ = profile.grant_access_to_path(&runs_dir);
        }
    }

    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let cnt = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let box_dir = runs_dir.join(format!("run-{ts}-{cnt}")).join("box");
    std::fs::create_dir_all(&box_dir)?;

    Ok(box_dir)
}

/// Forcibly removes a file, clearing readonly attributes if needed (e.g. on Windows).
#[allow(clippy::permissions_set_readonly_false)]
pub fn force_remove_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                let mut perms = metadata.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(path, perms);
                    return std::fs::remove_file(path);
                }
            }
            Err(e)
        }
    }
}

/// Forcibly removes a directory and all its contents, clearing readonly attributes if needed (e.g. on Windows).
#[allow(clippy::permissions_set_readonly_false)]
pub fn force_remove_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            fn make_writable_recursive(dir: &std::path::Path) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if let Ok(meta) = p.symlink_metadata() {
                            let mut perms = meta.permissions();
                            if perms.readonly() {
                                perms.set_readonly(false);
                                let _ = std::fs::set_permissions(&p, perms);
                            }
                            if meta.is_dir() {
                                make_writable_recursive(&p);
                            }
                        }
                    }
                }
            }
            if let Ok(meta) = path.symlink_metadata() {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(path, perms);
                }
            }
            make_writable_recursive(path);
            std::fs::remove_dir_all(path).or(Err(e))
        }
    }
}

/// Recursively removes all files and subdirectories within a box directory without deleting the box directory itself.
/// Returns Ok(true) if clean succeeded, or Err if any entry failed to delete.
pub fn clean_box_dir_contents(box_dir: &std::path::Path) -> Result<bool, std::io::Error> {
    let entries = std::fs::read_dir(box_dir)?;
    let mut any_err = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let res = if path.is_dir() {
            force_remove_dir_all(&path)
        } else {
            force_remove_file(&path)
        };
        if let Err(e) = res {
            any_err = Some(e);
        }
    }
    if let Some(e) = any_err {
        Err(e)
    } else {
        Ok(true)
    }
}

/// RAII Guard that manages an isolated slot box directory for a worker thread,
/// cleaning up the directory tree when the worker drops.
pub struct SlotBoxGuard {
    pub box_dir: std::path::PathBuf,
    pub run_dir: std::path::PathBuf,
}

impl Drop for SlotBoxGuard {
    fn drop(&mut self) {
        let _ = force_remove_dir_all(&self.run_dir);
    }
}

impl SlotBoxGuard {
    pub fn clean(&self) -> bool {
        clean_box_dir_contents(&self.box_dir).unwrap_or(false)
    }
}

/// Create an isolated, reusable slot box directory inside `<base_dir>/.uj/tmp/runs/slot-<pid>-<slot_id>-<ts>/box`.
/// This eliminates per-test `fs::create_dir_all` and `fs::remove_dir_all` overhead by pooling the box across test cases.
pub fn create_slot_box_dir(
    base_dir: &std::path::Path,
    slot_id: usize,
) -> Result<SlotBoxGuard, std::io::Error> {
    let runs_dir = base_dir.join(".uj").join("tmp").join("runs");
    std::fs::create_dir_all(&runs_dir)?;

    #[cfg(windows)]
    {
        let needs_auth = {
            let mut set = AUTHORIZED_RUNS_DIRS.lock();
            set.insert(runs_dir.clone())
        };
        if needs_auth && let Ok(profile) = windows::security::AppContainerProfile::get_shared() {
            let _ = profile.grant_access_to_path(&runs_dir);
        }
    }

    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let run_dir = runs_dir.join(format!("slot-{pid}-{slot_id}-{ts}"));
    let box_dir = run_dir.join("box");
    std::fs::create_dir_all(&box_dir)?;

    Ok(SlotBoxGuard { box_dir, run_dir })
}

type PreparedBinaryKey = (
    std::path::PathBuf,
    std::path::PathBuf,
    std::time::SystemTime,
    u64,
);

static PREPARED_BINARIES_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<PreparedBinaryKey, std::path::PathBuf>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

static AUTHORIZED_BINARIES: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

/// Prepares a shared copy of an executable in `<base_dir>/.uj/tmp/bin/<hash>.exe`.
/// The file is read, hashed, and written once per submission.
pub fn prepare_binary_content(
    exe_path: &std::path::Path,
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let meta_opt = std::fs::metadata(exe_path).ok();
    if let Some(ref meta) = meta_opt {
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = meta.len();
        let key = (exe_path.to_path_buf(), base_dir.to_path_buf(), mtime, size);
        let cache = PREPARED_BINARIES_CACHE.lock();
        if let Some(cached_auth) = cache.get(&key)
            && cached_auth.exists()
            && std::fs::metadata(cached_auth)
                .map(|m| m.len() == size)
                .unwrap_or(false)
        {
            return Ok(cached_auth.clone());
        }
    }

    let bytes = std::fs::read(exe_path)?;
    let hash = blake3::hash(&bytes).to_hex();
    let bin_dir = base_dir.join(".uj").join("tmp").join("bin");
    std::fs::create_dir_all(&bin_dir)?;

    #[cfg(windows)]
    let authorized_exe = bin_dir.join(format!("{}.exe", &hash[..16]));
    #[cfg(not(windows))]
    let authorized_exe = bin_dir.join(&hash[..16]);

    let need_write = if authorized_exe.exists() {
        std::fs::metadata(&authorized_exe)
            .map(|m| m.len() != bytes.len() as u64)
            .unwrap_or(true)
    } else {
        true
    };

    if need_write {
        static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let tmp_cnt = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp_exe = bin_dir.join(format!(
            "{}.tmp.{}.{}",
            &hash[..16],
            std::process::id(),
            tmp_cnt
        ));
        std::fs::write(&temp_exe, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp_exe, std::fs::Permissions::from_mode(0o755));
        }
        if let Err(_rename_err) = std::fs::rename(&temp_exe, &authorized_exe) {
            let _ = force_remove_file(&temp_exe);
            if !authorized_exe.exists()
                || std::fs::metadata(&authorized_exe)
                    .map(|m| m.len() != bytes.len() as u64)
                    .unwrap_or(true)
            {
                std::fs::write(&authorized_exe, &bytes)?;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&authorized_exe, std::fs::Permissions::from_mode(0o755));
    }

    if let Some(ref meta) = meta_opt {
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = meta.len();
        let key = (exe_path.to_path_buf(), base_dir.to_path_buf(), mtime, size);
        let mut cache = PREPARED_BINARIES_CACHE.lock();
        cache.insert(key, authorized_exe.clone());
    }

    Ok(authorized_exe)
}

/// Prepares a shared authorized copy of a binary in `<base_dir>/.uj/tmp/bin/<hash>.exe`
/// (authorized and prewarmed once, reused across all test cases).
pub fn get_or_prepare_authorized_binary(
    exe_path: &std::path::Path,
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let authorized_exe = prepare_binary_content(exe_path, base_dir)?;

    let needs_prep = {
        let mut set = AUTHORIZED_BINARIES.lock();
        set.insert(authorized_exe.clone())
    };

    if needs_prep {
        #[cfg(windows)]
        {
            if let Ok(profile) = windows::security::AppContainerProfile::get_shared() {
                let _ = profile.grant_access_to_path(&authorized_exe);
            }
            if let Ok(box_dir) = create_isolated_box_dir(base_dir) {
                prewarm_binary(&authorized_exe, &box_dir);
                if let Some(run_dir) = box_dir.parent() {
                    let _ = std::fs::remove_dir_all(run_dir);
                }
            }
        }
    }

    Ok(authorized_exe)
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
pub fn prewarm_binary(_exe_path: &std::path::Path, _work_dir: &std::path::Path) {}

#[cfg(not(windows))]
pub fn detect_p_core_primary_cpus() -> Vec<usize> {
    (0..num_cpus::get()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_force_remove_readonly_file() {
        let temp_dir = std::env::temp_dir().join(format!("uj_test_ro_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_file = temp_dir.join("readonly.txt");
        std::fs::write(&test_file, b"test content").unwrap();

        let mut perms = std::fs::metadata(&test_file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&test_file, perms).unwrap();

        assert!(force_remove_file(&test_file).is_ok());
        assert!(!test_file.exists());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_force_remove_readonly_dir_all() {
        let temp_dir = std::env::temp_dir().join(format!("uj_test_ro_dir_{}", std::process::id()));
        let sub_dir = temp_dir.join("sub");
        let _ = std::fs::create_dir_all(&sub_dir);
        let test_file = sub_dir.join("readonly.txt");
        std::fs::write(&test_file, b"test content").unwrap();

        let mut perms = std::fs::metadata(&test_file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&test_file, perms).unwrap();

        assert!(force_remove_dir_all(&temp_dir).is_ok());
        assert!(!temp_dir.exists());
    }

    #[test]
    fn test_clean_box_dir_contents_with_readonly() {
        let temp_dir =
            std::env::temp_dir().join(format!("uj_test_clean_ro_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_file = temp_dir.join("readonly.txt");
        std::fs::write(&test_file, b"test content").unwrap();

        let mut perms = std::fs::metadata(&test_file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&test_file, perms).unwrap();

        assert!(clean_box_dir_contents(&temp_dir).is_ok());
        assert!(temp_dir.exists());
        assert!(!test_file.exists());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

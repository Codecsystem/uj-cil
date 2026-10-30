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
    pub star_manager_proc: Option<usize>,
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

/// Create an isolated, unique box directory inside `<base_dir>/.uj/tmp/runs/run-<id>/box`.
/// On Windows, per-slot AppContainer ACLs are applied only to the specific `box` leaf directory
/// by `WindowsSandbox` when a test runs, preventing cross-slot or parent directory access.
pub fn create_isolated_box_dir(
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let runs_dir = base_dir.join(".uj").join("tmp").join("runs");
    std::fs::create_dir_all(&runs_dir)?;

    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let cnt = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let box_dir = runs_dir.join(format!("run-{pid}-{ts}-{cnt}")).join("box");
    std::fs::create_dir_all(&box_dir)?;

    Ok(box_dir)
}

/// Clear the read-only flag of `path` itself, never following symlinks / junctions.
///
/// `std::fs::set_permissions` resolves symlinks, so for a contestant-planted link it would
/// change the permissions of the (possibly outside-the-box) target. Links are therefore
/// handled through a handle opened on the reparse point itself (Windows) or skipped
/// (Unix: symlink permission bits are meaningless and never block unlinking).
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly_nofollow(path: &std::path::Path, meta: &std::fs::Metadata) {
    let mut perms = meta.permissions();
    if !perms.readonly() {
        return;
    }
    perms.set_readonly(false);
    if meta.file_type().is_symlink() {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            if let Ok(f) = std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
            {
                let _ = f.set_permissions(perms);
            }
        }
        return;
    }
    let _ = std::fs::set_permissions(path, perms);
}

/// Remove a symlink / junction itself (never its target).
fn remove_link_nofollow(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows directory symlinks and junctions must be removed with RemoveDirectory
            // (which deletes the reparse point, not the target directory).
            #[cfg(windows)]
            if std::fs::remove_dir(path).is_ok() {
                return Ok(());
            }
            if let Ok(meta) = std::fs::symlink_metadata(path)
                && meta.permissions().readonly()
            {
                clear_readonly_nofollow(path, &meta);
                #[cfg(windows)]
                if std::fs::remove_dir(path).is_ok() {
                    return Ok(());
                }
                return std::fs::remove_file(path);
            }
            Err(e)
        }
    }
}

/// Forcibly removes a file, clearing readonly attributes if needed (e.g. on Windows).
/// Symlinks are removed themselves; their targets are never touched.
pub fn force_remove_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                if metadata.file_type().is_symlink() {
                    return remove_link_nofollow(path);
                }
                if metadata.permissions().readonly() {
                    clear_readonly_nofollow(path, &metadata);
                    return std::fs::remove_file(path);
                }
            }
            Err(e)
        }
    }
}

/// Forcibly removes a directory and all its contents, clearing readonly attributes if needed (e.g. on Windows).
/// Never follows symlinks / junctions: links inside the tree are removed, not traversed.
pub fn force_remove_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            fn make_writable_recursive(dir: &std::path::Path) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        // `symlink_metadata` / `file_type` never follow links, so a planted
                        // link cannot redirect the chmod (or the recursion) outside the box.
                        if let Ok(meta) = p.symlink_metadata() {
                            clear_readonly_nofollow(&p, &meta);
                            if meta.file_type().is_dir() {
                                make_writable_recursive(&p);
                            }
                        }
                    }
                }
            }
            if let Ok(meta) = path.symlink_metadata() {
                if meta.file_type().is_symlink() {
                    return remove_link_nofollow(path).or(Err(e));
                }
                clear_readonly_nofollow(path, &meta);
            }
            make_writable_recursive(path);
            std::fs::remove_dir_all(path).or(Err(e))
        }
    }
}

/// Recursively removes all files and subdirectories within a box directory without deleting the box directory itself.
/// Returns Ok(true) if clean succeeded, or Err if any entry failed to delete.
///
/// Uses `DirEntry::file_type()` (no extra `stat`, does not follow symlinks): symlinks and
/// junctions planted by the contestant are unlinked, never traversed.
pub fn clean_box_dir_contents(box_dir: &std::path::Path) -> Result<bool, std::io::Error> {
    let entries = std::fs::read_dir(box_dir)?;
    let mut any_err = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => Some(ft),
            Err(_) => std::fs::symlink_metadata(&path).ok().map(|m| m.file_type()),
        };
        let res = match file_type {
            Some(ft) if ft.is_symlink() => remove_link_nofollow(&path),
            Some(ft) if ft.is_dir() => force_remove_dir_all(&path),
            _ => force_remove_file(&path),
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
/// On Windows, the owning `WindowsSandbox` slot grants its exclusive per-slot AppContainer SID
/// access only to the leaf `box` directory (never `runs` or `run_dir`).
pub fn create_slot_box_dir(
    base_dir: &std::path::Path,
    slot_id: usize,
) -> Result<SlotBoxGuard, std::io::Error> {
    let runs_dir = base_dir.join(".uj").join("tmp").join("runs");
    std::fs::create_dir_all(&runs_dir)?;

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

/// Identity of a file version used to memoize content hashes: path, mtime, size plus
/// platform identity (`dev`/`ino`/`ctime` on Unix, creation/last-write FILETIME on Windows).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileVersion {
    path: std::path::PathBuf,
    mtime: std::time::SystemTime,
    size: u64,
    extra: (u64, u64, i64, i64),
}

impl FileVersion {
    fn of(path: &std::path::Path, meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let extra = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
        };
        #[cfg(windows)]
        let extra = {
            use std::os::windows::fs::MetadataExt;
            (meta.creation_time(), meta.last_write_time(), 0, 0)
        };
        #[cfg(not(any(unix, windows)))]
        let extra = (0, 0, 0, 0);
        Self {
            path: path.to_path_buf(),
            mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            size: meta.len(),
            extra,
        }
    }
}

/// Process-wide memo of blake3 content hashes keyed by `FileVersion`, so repeated
/// `prepare_binary_content` calls for a freshly compiled binary (< 2 s old, which bypasses
/// `PREPARED_BINARIES_CACHE`) do not re-read and re-hash the whole file on every test.
static CONTENT_HASH_MEMO: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<FileVersion, String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Minimum distance between a file's mtime and the moment its metadata was sampled before
/// a memoized hash is trusted ("racy-git" rule): any later rewrite must then carry a newer
/// mtime, so a same-size rewrite within one timestamp tick cannot alias the memo entry.
const HASH_MEMO_RACY_MARGIN: std::time::Duration = std::time::Duration::from_millis(50);

fn hash_memo_is_safe(mtime: std::time::SystemTime, sampled_at: std::time::SystemTime) -> bool {
    let Ok(since_epoch) = mtime.duration_since(std::time::SystemTime::UNIX_EPOCH) else {
        return false;
    };
    // A zero sub-second part indicates a coarse-timestamp filesystem (FAT: 2 s, HFS+: 1 s);
    // there the 2-second guard of `PREPARED_BINARIES_CACHE` is the only safe rule.
    if since_epoch.subsec_nanos() == 0 {
        return false;
    }
    sampled_at
        .duration_since(mtime)
        .is_ok_and(|d| d >= HASH_MEMO_RACY_MARGIN)
}

fn authorized_binary_path(bin_dir: &std::path::Path, hash_hex: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        bin_dir.join(format!("{}.exe", &hash_hex[..16]))
    }
    #[cfg(not(windows))]
    {
        bin_dir.join(&hash_hex[..16])
    }
}

/// Prepares a shared copy of an executable in `<base_dir>/.uj/tmp/bin/<hash>.exe`.
/// The file is read, hashed, and written once per submission.
pub fn prepare_binary_content(
    exe_path: &std::path::Path,
    base_dir: &std::path::Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    let sampled_at = std::time::SystemTime::now();
    let meta_opt = std::fs::metadata(exe_path).ok();
    if let Some(ref meta) = meta_opt {
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = meta.len();
        // Sub-second jitter protection: if modified within the last 2 seconds, do not blindly
        // trust mtime/size alone without content hash verification.
        let is_recent = mtime.elapsed().map(|d| d.as_secs() < 2).unwrap_or(true);
        if !is_recent {
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
    }

    let bin_dir = base_dir.join(".uj").join("tmp").join("bin");
    let version = meta_opt.as_ref().map(|m| FileVersion::of(exe_path, m));

    // Memoized content hash (see `hash_memo_is_safe`): reuse the shared copy if it is intact.
    if let Some(ref v) = version
        && let Some(hash) = CONTENT_HASH_MEMO.lock().get(v).cloned()
    {
        let authorized_exe = authorized_binary_path(&bin_dir, &hash);
        if std::fs::metadata(&authorized_exe)
            .map(|m| m.len() == v.size)
            .unwrap_or(false)
        {
            return Ok(authorized_exe);
        }
    }

    let bytes = std::fs::read(exe_path)?;
    let hash = blake3::hash(&bytes).to_hex();
    std::fs::create_dir_all(&bin_dir)?;

    if let Some(ref v) = version
        && hash_memo_is_safe(v.mtime, sampled_at)
        && v.size == bytes.len() as u64
        && std::fs::metadata(exe_path)
            .map(|m| FileVersion::of(exe_path, &m) == *v)
            .unwrap_or(false)
    {
        CONTENT_HASH_MEMO
            .lock()
            .insert(v.clone(), hash.as_str().to_string());
    }

    let authorized_exe = authorized_binary_path(&bin_dir, hash.as_str());

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

/// Prepares a shared copy of a binary in `<base_dir>/.uj/tmp/bin/<hash>.exe`
/// and prewarms the image section (without executing untrusted code) once.
/// Per-slot read+execute (`RX`) ACLs are granted by each `WindowsSandbox` slot.
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
            let work_dir = authorized_exe.parent().unwrap_or(base_dir);
            prewarm_binary(&authorized_exe, work_dir);
        }
    }

    Ok(authorized_exe)
}

/// Fallback scratch directory for temp files
///
/// The path is computed once per process. The directory is (re)created at most once every
/// `SCRATCH_REVALIDATE_SECS` instead of on every call (it used to run `temp_dir()` +
/// `create_dir_all` per call), which still recovers if a temp cleaner removes it while a
/// long-running process (e.g. `uj mcp`) is alive.
pub fn get_sandbox_scratch_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicU64;
    const SCRATCH_REVALIDATE_SECS: u64 = 5;
    static SCRATCH: OnceLock<std::path::PathBuf> = OnceLock::new();
    static LAST_ENSURED: AtomicU64 = AtomicU64::new(0);

    let p = SCRATCH.get_or_init(|| std::env::temp_dir().join("uj").join("tmp"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_ENSURED.load(Ordering::Relaxed);
    if last == 0 || now.saturating_sub(last) >= SCRATCH_REVALIDATE_SECS {
        let _ = std::fs::create_dir_all(p);
        LAST_ENSURED.store(now.max(1), Ordering::Relaxed);
    }
    p.clone()
}

#[cfg(windows)]
pub use windows::process::{detect_p_core_primary_cpus, prewarm_binary};

#[cfg(not(windows))]
pub fn prewarm_binary(_exe_path: &std::path::Path, _work_dir: &std::path::Path) {}

/// Parse Linux `/sys/devices/system/cpu` topology to identify primary threads of physical P-cores,
/// filtering out SMT sibling threads and lower-capacity/lower-frequency E-cores.
pub fn parse_linux_p_core_primary_cpus(sysfs_cpu_dir: &std::path::Path) -> Option<Vec<usize>> {
    let entries = std::fs::read_dir(sysfs_cpu_dir).ok()?;
    struct CoreCandidate {
        cpu_id: usize,
        capacity: u64,
        max_freq: u64,
    }
    let mut candidates = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(id_str) = name_str.strip_prefix("cpu") else {
            continue;
        };
        let Ok(cpu_id) = id_str.parse::<usize>() else {
            continue;
        };

        let cpu_path = entry.path();
        // Skip offline CPUs if `online` file exists and is "0"
        if let Ok(online) = std::fs::read_to_string(cpu_path.join("online"))
            && online.trim() == "0"
        {
            continue;
        }

        // Check `topology/thread_siblings_list` to keep only the primary thread per physical core
        if let Ok(siblings) =
            std::fs::read_to_string(cpu_path.join("topology").join("thread_siblings_list"))
        {
            let first_sibling = siblings
                .trim()
                .split([',', '-'])
                .next()
                .and_then(|s| s.trim().parse::<usize>().ok());
            if let Some(primary_id) = first_sibling
                && primary_id != cpu_id
            {
                continue;
            }
        }

        let capacity = std::fs::read_to_string(cpu_path.join("cpu_capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let max_freq = std::fs::read_to_string(cpu_path.join("cpufreq").join("cpuinfo_max_freq"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);

        candidates.push(CoreCandidate {
            cpu_id,
            capacity,
            max_freq,
        });
    }

    if candidates.is_empty() {
        return None;
    }

    let max_cap = candidates.iter().map(|c| c.capacity).max().unwrap_or(0);
    let min_cap = candidates.iter().map(|c| c.capacity).min().unwrap_or(0);
    let max_freq = candidates.iter().map(|c| c.max_freq).max().unwrap_or(0);

    let mut result: Vec<usize> = candidates
        .into_iter()
        .filter(|c| {
            if max_cap > 0 && max_cap != min_cap {
                // Hybrid capacity reported (e.g. 1024 for P-core vs 609 for E-core)
                c.capacity == max_cap
            } else if max_freq > 0 {
                // Keep cores within 85% of peak turbo frequency (retains non-TVB P-cores, filters E-cores)
                c.max_freq.saturating_mul(100) >= max_freq.saturating_mul(85)
            } else {
                true
            }
        })
        .map(|c| c.cpu_id)
        .collect();

    if result.is_empty() {
        return None;
    }
    result.sort_unstable();
    Some(result)
}

#[cfg(not(windows))]
pub fn detect_p_core_primary_cpus() -> Vec<usize> {
    parse_linux_p_core_primary_cpus(std::path::Path::new("/sys/devices/system/cpu"))
        .unwrap_or_else(|| (0..num_cpus::get()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_linux_hybrid_and_smt_sysfs() {
        let temp_dir =
            std::env::temp_dir().join(format!("uj_test_sysfs_topo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Simulate 4 P-cores with SMT (cpus 0..8, pairs (0,1), (2,3), (4,5), (6,7))
        for p in 0..4 {
            let primary = p * 2;
            let sibling = p * 2 + 1;
            for cid in [primary, sibling] {
                let cdir = temp_dir.join(format!("cpu{cid}"));
                std::fs::create_dir_all(cdir.join("topology")).unwrap();
                std::fs::create_dir_all(cdir.join("cpufreq")).unwrap();
                std::fs::write(
                    cdir.join("topology").join("thread_siblings_list"),
                    format!("{primary},{sibling}\n"),
                )
                .unwrap();
                std::fs::write(cdir.join("cpu_capacity"), "1024\n").unwrap();
                std::fs::write(cdir.join("cpufreq").join("cpuinfo_max_freq"), "5500000\n").unwrap();
            }
        }
        // Simulate 4 E-cores without SMT (cpus 8..12)
        for cid in 8..12 {
            let cdir = temp_dir.join(format!("cpu{cid}"));
            std::fs::create_dir_all(cdir.join("topology")).unwrap();
            std::fs::create_dir_all(cdir.join("cpufreq")).unwrap();
            std::fs::write(
                cdir.join("topology").join("thread_siblings_list"),
                format!("{cid}\n"),
            )
            .unwrap();
            std::fs::write(cdir.join("cpu_capacity"), "609\n").unwrap();
            std::fs::write(cdir.join("cpufreq").join("cpuinfo_max_freq"), "4000000\n").unwrap();
        }

        let p_cores = parse_linux_p_core_primary_cpus(&temp_dir).unwrap();
        assert_eq!(p_cores, vec![0, 2, 4, 6]);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

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

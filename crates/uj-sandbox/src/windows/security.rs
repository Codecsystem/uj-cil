//! AppContainer and token isolation for Windows sandbox.
//!
//! Directory / executable ACL grants for the per-slot AppContainer SID are applied
//! in-process through the Win32 security APIs (`GetNamedSecurityInfoW` +
//! `SetEntriesInAclW` + `SetNamedSecurityInfoW`) instead of spawning `icacls.exe`
//! (Round 14: ~37 ms per spawn → sub-millisecond).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    ACCESS_MODE, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, FreeSid, GetSecurityDescriptorControl,
    NO_INHERITANCE, OBJECT_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
};
use windows_core::{PCWSTR, PWSTR};

pub struct AppContainerProfile {
    sid: PSID,
    name: String,
    is_transient: bool,
    /// Cached textual SID (`S-1-15-2-...`), computed on first use.
    sid_str: OnceLock<String>,
}

// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Send for AppContainerProfile {}
// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Sync for AppContainerProfile {}

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

static SHARED_PROFILE: OnceLock<Result<Arc<AppContainerProfile>, String>> = OnceLock::new();
static PROFILE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Serializes read-modify-write DACL updates inside this process. Several worker slots may
/// grant `RX` on the same shared `.uj/tmp/bin/<hash>.exe` concurrently; without the lock two
/// interleaved `Get → SetEntriesInAcl → Set` sequences would lose one slot's ACE.
static ACL_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// `PROTECTED_DACL_SECURITY_INFORMATION` (winnt.h).
const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

/// Full control (`icacls ... F`).
const ACCESS_FULL: u32 = FILE_ALL_ACCESS.0;
/// Read + execute (`icacls ... RX`).
const ACCESS_READ_EXECUTE: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;

/// Frees a `LocalAlloc`-ed buffer returned by the security APIs on drop.
struct LocalAllocGuard(*mut core::ffi::c_void);

impl Drop for LocalAllocGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }
}

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn win32_io_error(code: WIN32_ERROR, what: &str, path: &Path) -> std::io::Error {
    let os = std::io::Error::from_raw_os_error(code.0 as i32);
    std::io::Error::new(
        os.kind(),
        format!("{what} failed for '{}': {os}", path.display()),
    )
}

/// Read the DACL protection bit of an object (`SE_DACL_PROTECTED`).
fn dacl_is_protected(path: &Path) -> std::io::Result<bool> {
    let wide = to_wide(path);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut psd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_io_error(rc, "GetNamedSecurityInfoW", path));
    }
    let _sd_guard = LocalAllocGuard(psd.0);
    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    unsafe {
        GetSecurityDescriptorControl(psd, &mut control, &mut revision)
            .map_err(|e| std::io::Error::other(format!("GetSecurityDescriptorControl: {e}")))?;
    }
    Ok(control & SE_DACL_PROTECTED.0 != 0)
}

/// Merge a single explicit ACE for `sid` into the DACL of `path` (one object; the system
/// automatically propagates inheritable ACEs to non-protected children).
///
/// Must be called with `ACL_LOCK` held.
fn apply_explicit_ace_locked(
    path: &Path,
    sid: PSID,
    mask: u32,
    mode: ACCESS_MODE,
    inheritance: ACE_FLAGS,
) -> std::io::Result<()> {
    let wide = to_wide(path);
    let mut old_dacl: *mut ACL = null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut psd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_io_error(rc, "GetNamedSecurityInfoW", path));
    }
    let _sd_guard = LocalAllocGuard(psd.0);

    // Preserve the existing protection state of the DACL.
    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    let protected = unsafe { GetSecurityDescriptorControl(psd, &mut control, &mut revision) }
        .is_ok()
        && (control & SE_DACL_PROTECTED.0) != 0;

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0 as *mut u16),
        },
    };
    let mut new_dacl: *mut ACL = null_mut();
    let old_opt = if old_dacl.is_null() {
        None
    } else {
        Some(old_dacl as *const ACL)
    };
    let rc = unsafe { SetEntriesInAclW(Some(&[ea]), old_opt, &mut new_dacl) };
    if rc != ERROR_SUCCESS {
        return Err(win32_io_error(rc, "SetEntriesInAclW", path));
    }
    let _acl_guard = LocalAllocGuard(new_dacl as *mut _);

    let mut info = DACL_SECURITY_INFORMATION.0;
    if protected {
        info |= PROTECTED_DACL_SECURITY_INFORMATION;
    }
    let rc = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OBJECT_SECURITY_INFORMATION(info),
            PSID::default(),
            PSID::default(),
            Some(new_dacl as *const ACL),
            None,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_io_error(rc, "SetNamedSecurityInfoW", path));
    }
    Ok(())
}

/// Equivalent of `icacls <dir> /grant *SID:(OI)(CI)F /T /C`:
/// adds an inheritable full-control ACE on `dir` (the system propagates it to every
/// non-protected descendant) and adds explicit ACEs to descendants whose DACL is
/// protected (they would not receive the inherited ACE).
///
/// Reparse points (symlinks / junctions) inside the tree are never followed.
fn grant_tree_full_locked(dir: &Path, sid: PSID) -> std::io::Result<()> {
    let root_inherit = if dir.is_dir() {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    apply_explicit_ace_locked(dir, sid, ACCESS_FULL, GRANT_ACCESS, root_inherit)?;
    if root_inherit == NO_INHERITANCE {
        return Ok(());
    }
    grant_protected_descendants_locked(dir, sid)
}

fn grant_protected_descendants_locked(dir: &Path, sid: PSID) -> std::io::Result<()> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            // Never follow (or modify the target of) reparse points.
            continue;
        }
        let p = entry.path();
        if dacl_is_protected(&p)? {
            let inherit = if ft.is_dir() {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            };
            apply_explicit_ace_locked(&p, sid, ACCESS_FULL, GRANT_ACCESS, inherit)?;
        }
        if ft.is_dir() {
            grant_protected_descendants_locked(&p, sid)?;
        }
    }
    Ok(())
}

/// Owned SID parsed from its string form (freed with `LocalFree`).
struct OwnedStringSid(PSID);

impl OwnedStringSid {
    fn parse(sid_str: &str) -> std::io::Result<Self> {
        let wide: Vec<u16> = sid_str.encode_utf16().chain(Some(0)).collect();
        let mut psid = PSID::default();
        unsafe {
            ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid SID string '{sid_str}': {e}"),
                )
            })?;
        }
        Ok(Self(psid))
    }
}

impl Drop for OwnedStringSid {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0.0));
            }
        }
    }
}

/// Refuse to hand an untrusted AppContainer a recursive write ACL on a problem directory.
fn ensure_not_problem_dir(path: &Path) -> std::io::Result<()> {
    let is_problem_dir = path.join("problem.toml").exists();
    let is_problem_tests_dir = path.file_name().is_some_and(|n| n == "tests")
        && path
            .parent()
            .is_some_and(|p| p.join("problem.toml").exists());
    if is_problem_dir || is_problem_tests_dir {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "Security violation: refusing to grant untrusted AppContainer recursive write ACL to problem directory '{}'",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Grant `sid` recursive full control on `path` (Fail-Closed: any error is returned).
pub fn grant_path_access_sid(path: &Path, sid: PSID) -> std::io::Result<()> {
    ensure_not_problem_dir(path)?;
    let _g = ACL_LOCK.lock();
    grant_tree_full_locked(path, sid)
}

/// Grant `sid` read + execute on a single file (`icacls <path> /grant *SID:RX`).
pub fn grant_path_read_execute_sid(path: &Path, sid: PSID) -> std::io::Result<()> {
    let _g = ACL_LOCK.lock();
    apply_explicit_ace_locked(path, sid, ACCESS_READ_EXECUTE, GRANT_ACCESS, NO_INHERITANCE)
}

/// Remove every explicit allow ACE for `sid` from `path` (`icacls <path> /remove *SID`).
/// Inherited copies on descendants disappear through automatic propagation.
pub fn revoke_path_access_sid(path: &Path, sid: PSID) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _g = ACL_LOCK.lock();
    apply_explicit_ace_locked(path, sid, 0, REVOKE_ACCESS, NO_INHERITANCE)
}

impl AppContainerProfile {
    pub const DEFAULT_NAME: &'static str = "uj.sandbox";

    /// Create an isolated profile unique to this sandbox instance / worker slot.
    pub fn create_isolated() -> Result<Self, std::io::Error> {
        let count = PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("uj.sandbox.{}.iso.{}", std::process::id(), count);
        Self::create_internal(&name, true)
    }

    /// Create an isolated profile unique to this specific worker slot.
    pub fn create_isolated_for_slot(slot_id: usize) -> Result<Self, std::io::Error> {
        let count = PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "uj.sandbox.{}.slot.{}.{}",
            std::process::id(),
            slot_id,
            count
        );
        Self::create_internal(&name, true)
    }

    /// Get or initialize the shared AppContainer profile.
    pub fn get_shared() -> Result<Arc<Self>, std::io::Error> {
        let cached = SHARED_PROFILE.get_or_init(|| {
            Self::create_internal(Self::DEFAULT_NAME, false)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        });

        match cached {
            Ok(profile) => Ok(Arc::clone(profile)),
            Err(e) => Err(std::io::Error::other(e.clone())),
        }
    }

    /// Initialize or derive AppContainer SID.
    pub fn get_or_create(name: Option<&str>) -> Result<Self, std::io::Error> {
        let name_str = name.unwrap_or(Self::DEFAULT_NAME);
        Self::create_internal(name_str, false)
    }

    fn create_internal(name_str: &str, is_transient: bool) -> Result<Self, std::io::Error> {
        let name_utf16: Vec<u16> = name_str.encode_utf16().chain(Some(0)).collect();
        let pcwstr = PCWSTR(name_utf16.as_ptr());

        let display_name: Vec<u16> = "uj sandbox profile\0".encode_utf16().collect();
        let desc: Vec<u16> = "Sandbox for uj competitive programming evaluation\0"
            .encode_utf16()
            .collect();

        // First attempt to create profile so it is properly registered with Windows
        let create_res = unsafe {
            CreateAppContainerProfile(
                pcwstr,
                PCWSTR(display_name.as_ptr()),
                PCWSTR(desc.as_ptr()),
                None,
            )
        };

        match create_res {
            Ok(sid) => Ok(Self {
                sid,
                name: name_str.to_string(),
                is_transient,
                sid_str: OnceLock::new(),
            }),
            Err(e) => {
                // If profile already exists (HRESULT: 0x800700B7 or ERROR_ALREADY_EXISTS), derive existing SID
                let derive_res = unsafe { DeriveAppContainerSidFromAppContainerName(pcwstr) };
                if let Ok(sid) = derive_res
                    && !sid.is_invalid()
                {
                    return Ok(Self {
                        sid,
                        name: name_str.to_string(),
                        is_transient,
                        sid_str: OnceLock::new(),
                    });
                }
                Err(std::io::Error::from_raw_os_error(e.code().0))
            }
        }
    }

    pub fn sid(&self) -> PSID {
        self.sid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sid_string(&self) -> Result<String, std::io::Error> {
        if let Some(s) = self.sid_str.get() {
            return Ok(s.clone());
        }
        let s = Self::compute_sid_string(self.sid)?;
        Ok(self.sid_str.get_or_init(|| s).clone())
    }

    fn compute_sid_string(sid: PSID) -> Result<String, std::io::Error> {
        use windows::Win32::Security::Authorization::ConvertSidToStringSidW;

        let mut str_ptr = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(sid, &mut str_ptr)?;
        }
        let _guard = LocalAllocGuard(str_ptr.0 as *mut _);
        let s = unsafe {
            str_ptr
                .to_string()
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };
        Ok(s)
    }

    /// Grant this profile's SID recursive full control on `path` (in-process ACL update).
    pub fn grant_access_to_path(&self, path: &Path) -> Result<(), std::io::Error> {
        grant_path_access_sid(path, self.sid)
    }

    /// Grant this profile's SID read + execute on a single executable.
    pub fn grant_read_execute(&self, path: &Path) -> Result<(), std::io::Error> {
        grant_path_read_execute_sid(path, self.sid)
    }

    /// Remove this profile's explicit ACEs from `path`.
    pub fn revoke_access(&self, path: &Path) -> Result<(), std::io::Error> {
        revoke_path_access_sid(path, self.sid)
    }

    pub fn grant_path_access(path: &Path, sid_str: &str) -> Result<(), std::io::Error> {
        ensure_not_problem_dir(path)?;
        let sid = OwnedStringSid::parse(sid_str)?;
        grant_path_access_sid(path, sid.0)
    }

    pub fn grant_path_read_execute(path: &Path, sid_str: &str) -> Result<(), std::io::Error> {
        let sid = OwnedStringSid::parse(sid_str)?;
        grant_path_read_execute_sid(path, sid.0)
    }

    pub fn revoke_path_access(path: &Path, sid_str: &str) -> Result<(), std::io::Error> {
        if !path.exists() {
            return Ok(());
        }
        let sid = OwnedStringSid::parse(sid_str)?;
        revoke_path_access_sid(path, sid.0)
    }
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        if !self.sid.is_invalid() {
            unsafe {
                let _ = FreeSid(self.sid);
            }
        }
        if self.is_transient {
            let name_utf16: Vec<u16> = self.name.encode_utf16().chain(Some(0)).collect();
            let pcwstr = PCWSTR(name_utf16.as_ptr());
            unsafe {
                let _ = DeleteAppContainerProfile(pcwstr);
            }
        }
    }
}

/// Enumerates and removes leftover AppContainer profiles belonging to dead uj processes.
pub fn cleanup_stale_appcontainers() -> usize {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let subkey: Vec<u16> =
        "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppContainer\\Mappings\0"
            .encode_utf16()
            .collect();

    let mut hkey = HKEY::default();
    let res = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
    };
    if res.is_err() {
        return 0;
    }

    let mut cleaned = 0;
    let mut index = 0;
    let mut key_name = [0u16; 256];
    let mut to_delete = Vec::new();

    loop {
        let mut key_name_len = key_name.len() as u32;
        let enum_res = unsafe {
            RegEnumKeyExW(
                hkey,
                index,
                PWSTR(key_name.as_mut_ptr()),
                &mut key_name_len,
                None,
                PWSTR::null(),
                None,
                None,
            )
        };
        if enum_res.is_err() {
            break;
        }
        index += 1;

        let mut child_key = HKEY::default();
        let open_child =
            unsafe { RegOpenKeyExW(hkey, PCWSTR(key_name.as_ptr()), 0, KEY_READ, &mut child_key) };
        if open_child.is_ok() {
            let val_name: Vec<u16> = "Moniker\0".encode_utf16().collect();
            let mut val_data = [0u16; 256];
            let mut val_data_len = (val_data.len() * 2) as u32;
            let mut val_type = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
            let query_res = unsafe {
                RegQueryValueExW(
                    child_key,
                    PCWSTR(val_name.as_ptr()),
                    None,
                    Some(&mut val_type),
                    Some(val_data.as_mut_ptr() as *mut u8),
                    Some(&mut val_data_len),
                )
            };
            unsafe {
                let _ = RegCloseKey(child_key);
            }

            if query_res.is_ok() && val_type == REG_SZ {
                let moniker = String::from_utf16_lossy(&val_data[..(val_data_len as usize / 2)])
                    .trim_matches('\0')
                    .to_string();
                if moniker.starts_with("uj.sandbox.") {
                    let parts: Vec<&str> = moniker.split('.').collect();
                    if parts.len() >= 3
                        && let Ok(pid) = parts[2].parse::<u32>()
                    {
                        let is_dead = unsafe {
                            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
                            match handle {
                                Ok(h) => {
                                    let _ = CloseHandle(h);
                                    false
                                }
                                Err(_) => true,
                            }
                        };
                        if is_dead {
                            to_delete.push(moniker);
                        }
                    }
                }
            }
        }
    }

    unsafe {
        let _ = RegCloseKey(hkey);
    }

    for moniker in to_delete {
        let moniker_u16: Vec<u16> = moniker.encode_utf16().chain(Some(0)).collect();
        let pcwstr = PCWSTR(moniker_u16.as_ptr());
        if unsafe { DeleteAppContainerProfile(pcwstr).is_ok() } {
            cleaned += 1;
        }
    }

    cleaned
}

//! AppContainer and token isolation for Windows sandbox.

use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{FreeSid, PSID};
use windows_core::PCWSTR;

pub struct AppContainerProfile {
    sid: PSID,
    name: String,
    is_transient: bool,
}

// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Send for AppContainerProfile {}
// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Sync for AppContainerProfile {}

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

static SHARED_PROFILE: OnceLock<Result<Arc<AppContainerProfile>, String>> = OnceLock::new();
static PROFILE_COUNTER: AtomicU64 = AtomicU64::new(1);

impl AppContainerProfile {
    pub const DEFAULT_NAME: &'static str = "uj.sandbox";

    /// Create an isolated profile unique to this sandbox instance / worker slot.
    pub fn create_isolated() -> Result<Self, std::io::Error> {
        let count = PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("uj.sandbox.{}.{}", std::process::id(), count);
        Self::create_internal(&name, true)
    }

    /// Create an isolated profile unique to this specific worker slot.
    pub fn create_isolated_for_slot(slot_id: usize) -> Result<Self, std::io::Error> {
        let name = format!("uj.sandbox.{}.{}", std::process::id(), slot_id);
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
        use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_core::PWSTR;

        let mut str_ptr = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(self.sid, &mut str_ptr)?;
        }
        let s = unsafe {
            str_ptr
                .to_string()
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
                str_ptr.0 as *mut _,
            ));
        }
        Ok(s)
    }

    pub fn grant_access_to_path(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let sid_str = self.sid_string()?;
        Self::grant_path_access(path, &sid_str)
    }

    pub fn grant_path_access(path: &std::path::Path, sid_str: &str) -> Result<(), std::io::Error> {
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/grant")
            .arg(format!("*{sid_str}:(OI)(CI)F"))
            .arg("/T")
            .arg("/C")
            .arg("/Q")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        Ok(())
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
    use windows::Win32::Foundation::CloseHandle;
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
                windows_core::PWSTR(key_name.as_mut_ptr()),
                &mut key_name_len,
                None,
                windows_core::PWSTR::null(),
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

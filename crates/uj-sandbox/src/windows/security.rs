//! AppContainer and token isolation for Windows sandbox.

use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{FreeSid, PSID};
use windows_core::PCWSTR;

pub struct AppContainerProfile {
    sid: PSID,
    name: String,
}

// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Send for AppContainerProfile {}
// SAFETY: PSID points to an OS-allocated immutable SID structure that is safe to read across threads.
unsafe impl Sync for AppContainerProfile {}

use std::sync::{Arc, OnceLock};

static SHARED_PROFILE: OnceLock<Result<Arc<AppContainerProfile>, String>> = OnceLock::new();

impl AppContainerProfile {
    pub const DEFAULT_NAME: &'static str = "uj.sandbox";

    /// Get or initialize the shared AppContainer profile.
    pub fn get_shared() -> Result<Arc<Self>, std::io::Error> {
        let cached = SHARED_PROFILE.get_or_init(|| {
            Self::get_or_create(None)
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

    pub fn grant_path_access(
        path: &std::path::Path,
        sid_str: &str,
    ) -> Result<(), std::io::Error> {
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
    }
}

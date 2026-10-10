//! Job Object RAII wrapper for Windows sandbox execution.

use std::mem::size_of;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_AFFINITY, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_core::PCWSTR;

pub struct JobObject {
    handle: HANDLE,
}

impl JobObject {
    pub fn create(name: Option<&str>) -> Result<Self, std::io::Error> {
        let name_utf16: Option<Vec<u16>> = name.map(|s| s.encode_utf16().chain(Some(0)).collect());
        let pcwstr = name_utf16
            .as_ref()
            .map(|v| PCWSTR(v.as_ptr()))
            .unwrap_or(PCWSTR::null());

        let handle = unsafe { CreateJobObjectW(None, pcwstr)? };
        Ok(Self { handle })
    }

    pub fn set_limits(
        &self,
        memory_limit_bytes: u64,
        max_processes: u32,
        cpu_affinity: Option<&[usize]>,
    ) -> Result<(), std::io::Error> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();

        let mut flags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;

        if max_processes > 0 {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = max_processes;
        }

        if memory_limit_bytes > 0 {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = memory_limit_bytes as usize;
        }

        if let Some(cores) = cpu_affinity
            && !cores.is_empty()
        {
            flags |= JOB_OBJECT_LIMIT_AFFINITY;
            let mut mask: usize = 0;
            for &core in cores {
                if core < usize::BITS as usize {
                    mask |= 1 << core;
                }
            }
            info.BasicLimitInformation.Affinity = mask;
        }

        info.BasicLimitInformation.LimitFlags = flags;

        unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }

        /*
        let mut ui_restrictions = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
        // UI restrictions: handles, read/write clipboard, system parameters, display settings, global atoms, exit windows
        let safe_ui_limits: u32 = 0x02 | 0x04 | 0x10 | 0x80;
        ui_restrictions.UIRestrictionsClass = unsafe { std::mem::transmute(safe_ui_limits) };

        unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectBasicUIRestrictions,
                &ui_restrictions as *const _ as *const _,
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )?;
        }
        */

        Ok(())
    }

    pub fn assign_process(&self, process_handle: HANDLE) -> Result<(), std::io::Error> {
        unsafe {
            AssignProcessToJobObject(self.handle, process_handle)?;
        }
        Ok(())
    }

    pub fn terminate(&self, exit_code: u32) -> Result<(), std::io::Error> {
        unsafe {
            TerminateJobObject(self.handle, exit_code)?;
        }
        Ok(())
    }

    /// Query total user + kernel time in microseconds.
    pub fn query_cpu_time_us(&self) -> Result<u64, std::io::Error> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )?;
        }
        // TotalUserTime and TotalKernelTime are in 100-nanosecond intervals (LARGE_INTEGER = i64)
        let total_100ns = (info.TotalUserTime as u64).saturating_add(info.TotalKernelTime as u64);
        Ok(total_100ns / 10)
    }

    /// Query peak committed memory in bytes.
    pub fn query_peak_memory_bytes(&self) -> Result<u64, std::io::Error> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                None,
            )?;
        }
        Ok(info.PeakJobMemoryUsed as u64)
    }

    pub fn raw_handle(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

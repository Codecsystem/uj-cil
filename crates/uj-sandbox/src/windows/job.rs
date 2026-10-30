use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_AFFINITY, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOBOBJECT_LIMIT_VIOLATION_INFORMATION, JobObjectAssociateCompletionPortInformation,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    JobObjectLimitViolationInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_core::PCWSTR;

pub const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
pub const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
pub const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
pub const JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
pub const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10;

/// `WAIT_TIMEOUT` as returned by `GetLastError()` after a timed-out `GetQueuedCompletionStatus`.
const WAIT_TIMEOUT_CODE: i32 = 258;

pub struct JobObject {
    handle: HANDLE,
    iocp: HANDLE,
    /// Sticky flag: a memory-limit notification has been observed on the IOCP.
    mem_violation_seen: AtomicBool,
    /// Sticky flag: `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` has been observed on the IOCP.
    active_zero_seen: AtomicBool,
}

/// A notification dequeued from the job's I/O completion port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobMessage {
    /// `JOB_OBJECT_MSG_*` code.
    pub code: u32,
    /// Message-specific value (the process id for per-process messages).
    pub value: usize,
}

impl JobObject {
    pub fn create(name: Option<&str>) -> Result<Self, std::io::Error> {
        let name_utf16: Option<Vec<u16>> = name.map(|s| s.encode_utf16().chain(Some(0)).collect());
        let pcwstr = name_utf16
            .as_ref()
            .map(|v| PCWSTR(v.as_ptr()))
            .unwrap_or(PCWSTR::null());

        let handle = unsafe { CreateJobObjectW(None, pcwstr)? };

        // Create an I/O completion port and associate it with the job object
        // to receive notification messages (e.g. memory limit exceeded).
        let iocp = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1)? };

        let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: handle.0,
            CompletionPort: iocp,
        };

        let set_assoc_res = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectAssociateCompletionPortInformation,
                &assoc as *const _ as *const _,
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        };
        if let Err(e) = set_assoc_res {
            unsafe {
                let _ = CloseHandle(iocp);
                let _ = CloseHandle(handle);
            }
            return Err(e.into());
        }

        Ok(Self {
            handle,
            iocp,
            mem_violation_seen: AtomicBool::new(false),
            active_zero_seen: AtomicBool::new(false),
        })
    }

    fn record_message(&self, code: u32) {
        if code == JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT || code == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
            self.mem_violation_seen.store(true, Ordering::SeqCst);
        } else if code == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
            self.active_zero_seen.store(true, Ordering::SeqCst);
        }
    }

    /// Block for at most `timeout_ms` waiting for the next job notification
    /// (`EXIT_PROCESS`, `ACTIVE_PROCESS_ZERO`, memory-limit, ...).
    ///
    /// Returns `None` on timeout. Memory-limit and active-process-zero notifications are
    /// recorded in sticky flags so `check_memory_limit_violation` still observes them.
    /// Never busy-spins: if the port itself fails, the call sleeps for the slice instead.
    pub fn wait_message(&self, timeout_ms: u32) -> Option<JobMessage> {
        let mut msg_code: u32 = 0;
        let mut key: usize = 0;
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        let res = unsafe {
            GetQueuedCompletionStatus(
                self.iocp,
                &mut msg_code,
                &mut key,
                &mut overlapped,
                timeout_ms,
            )
        };
        match res {
            Ok(()) => {
                self.record_message(msg_code);
                Some(JobMessage {
                    code: msg_code,
                    value: overlapped as usize,
                })
            }
            Err(e) => {
                let is_timeout =
                    e.code() == windows_core::HRESULT::from_win32(WAIT_TIMEOUT_CODE as u32);
                if !is_timeout && overlapped.is_null() && timeout_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
                }
                None
            }
        }
    }

    /// Whether `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` has been observed.
    pub fn active_process_zero_seen(&self) -> bool {
        self.active_zero_seen.load(Ordering::SeqCst)
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
            let hard_limit = (memory_limit_bytes as usize).saturating_add(32 * 1024 * 1024);
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY
                | windows::Win32::System::JobObjects::JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.JobMemoryLimit = hard_limit;
            info.ProcessMemoryLimit = hard_limit;
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

        let ui_restrictions = windows::Win32::System::JobObjects::JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_HANDLES
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_READCLIPBOARD
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_GLOBALATOMS
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_DESKTOP
                | windows::Win32::System::JobObjects::JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };

        unsafe {
            let _ = SetInformationJobObject(
                self.handle,
                windows::Win32::System::JobObjects::JobObjectBasicUIRestrictions,
                &ui_restrictions as *const _ as *const _,
                size_of::<windows::Win32::System::JobObjects::JOBOBJECT_BASIC_UI_RESTRICTIONS>()
                    as u32,
            );
        }

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

    /// Query the number of currently active processes assigned to this job object.
    pub fn query_active_processes(&self) -> Result<u32, std::io::Error> {
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
        Ok(info.ActiveProcesses)
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

    /// Check if a memory limit violation has occurred via IOCP messages or query.
    pub fn check_memory_limit_violation(&self) -> bool {
        // 1. Drain and check IOCP notification messages
        let mut msg_code: u32 = 0;
        let mut key: usize = 0;
        let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
        while unsafe {
            GetQueuedCompletionStatus(
                self.iocp,
                &mut msg_code,
                &mut key,
                &mut overlapped,
                0, // non-blocking
            )
        }
        .is_ok()
        {
            self.record_message(msg_code);
        }
        if self.mem_violation_seen.load(Ordering::SeqCst) {
            return true;
        }

        // 2. Query JobObjectLimitViolationInformation
        let mut violation = JOBOBJECT_LIMIT_VIOLATION_INFORMATION::default();
        let query_res = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectLimitViolationInformation,
                &mut violation as *mut _ as *mut _,
                size_of::<JOBOBJECT_LIMIT_VIOLATION_INFORMATION>() as u32,
                None,
            )
        };
        if query_res.is_ok() {
            let flags = violation.ViolationLimitFlags;
            let mem_flags = JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            if (flags.0 & mem_flags.0) != 0 {
                return true;
            }
        }

        false
    }

    pub fn raw_handle(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        if !self.iocp.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.iocp);
            }
        }
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

//! Process creation and execution monitoring for Windows sandbox.

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::SECURITY_CAPABILITIES;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    ResumeThread, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_core::{PCWSTR, PWSTR};

use crate::ntstatus::is_ntstatus_exception;
use crate::windows::job::JobObject;
use crate::windows::security::AppContainerProfile;
use crate::{CancelToken, SandboxError};

// Thread attribute keys:
pub const ATTR_HANDLE_LIST: usize = 0x00020002;
pub const ATTR_SECURITY_CAPABILITIES: usize = 0x00020009;
pub const ATTR_CHILD_PROCESS_POLICY: usize = 0x0002000E;
pub const PROCESS_CREATION_CHILD_PROCESS_RESTRICTED: u32 = 0x01;

pub fn execute_in_job(
    spec: &RunSpec,
    job: &JobObject,
    app_container: Option<&AppContainerProfile>,
    cancel: &CancelToken,
    tier: Tier,
) -> Result<RunResult, SandboxError> {
    if spec.argv.is_empty() {
        return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
    }

    // Resolve executable path
    let exe_path =
        which::which(&spec.argv[0]).unwrap_or_else(|_| std::path::PathBuf::from(&spec.argv[0]));
    let mut full_argv = spec.argv.clone();
    full_argv[0] = exe_path.clone().into_os_string();

    // Build command line string (with proper escaping for Windows)
    let cmd_line_str = build_windows_cmd_line(&full_argv);
    let mut cmd_line_utf16: Vec<u16> = cmd_line_str.encode_utf16().chain(Some(0)).collect();

    let work_dir_utf16: Vec<u16> = spec
        .work_dir
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let pcwstr_work_dir = PCWSTR(work_dir_utf16.as_ptr());

    // Prepare pipes / file handles for stdin, stdout, stderr
    let (h_stdin, stdin_cleaner) = prepare_stdin(&spec.stdin)?;
    let (h_stdout, stdout_read_handle, stdout_cleaner) = prepare_stdout(&spec.stdout)?;
    let (h_stderr, stderr_read_handle, stderr_cleaner) = prepare_stderr(&spec.stderr)?;

    let mut inherit_handles = vec![h_stdin, h_stdout, h_stderr];

    let mut attr_count = 1; // At least ATTR_HANDLE_LIST
    if app_container.is_some() {
        attr_count += 1;
    }

    let mut attr_size: usize = 0;
    unsafe {
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST(null_mut()),
            attr_count,
            0,
            &mut attr_size,
        );
    }

    let mut attr_buffer: Vec<u8> = vec![0u8; attr_size];
    let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buffer.as_mut_ptr() as *mut _);

    unsafe {
        InitializeProcThreadAttributeList(attr_list, attr_count, 0, &mut attr_size).map_err(
            |e| SandboxError::Internal(format!("InitializeProcThreadAttributeList failed: {e}")),
        )?;
    }

    // 1. Whitelist handle inheritance (strictly limits inherited handles to stdin, stdout, stderr)
    unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            ATTR_HANDLE_LIST,
            Some(inherit_handles.as_mut_ptr() as *mut _),
            inherit_handles.len() * size_of::<HANDLE>(),
            None,
            None,
        )
        .map_err(|e| {
            SandboxError::Internal(format!(
                "UpdateProcThreadAttribute (HANDLE_LIST) failed: {e}"
            ))
        })?;
    }

    // 2. Security capabilities (AppContainer sandbox isolation)
    let mut sec_caps = SECURITY_CAPABILITIES::default();
    if let Some(ac) = app_container {
        sec_caps.AppContainerSid = ac.sid();
        unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                ATTR_SECURITY_CAPABILITIES,
                Some(&mut sec_caps as *mut _ as *mut _),
                size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
            .map_err(|e| {
                SandboxError::Internal(format!(
                    "UpdateProcThreadAttribute (SECURITY_CAPABILITIES) failed: {e}"
                ))
            })?;
        }
    }

    let mut si_ex = STARTUPINFOEXW::default();
    si_ex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si_ex.StartupInfo.dwFlags = windows::Win32::System::Threading::STARTF_USESTDHANDLES;
    si_ex.StartupInfo.hStdInput = h_stdin;
    si_ex.StartupInfo.hStdOutput = h_stdout;
    si_ex.StartupInfo.hStdError = h_stderr;
    si_ex.lpAttributeList = attr_list;

    // Build minimal environment block
    let env_block = build_environment_block(&spec.env, &spec.work_dir);

    let mut pi = PROCESS_INFORMATION::default();

    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT
        | CREATE_NO_WINDOW
        | CREATE_SUSPENDED;

    let app_name_utf16: Vec<u16> = exe_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let pcwstr_app_name = PCWSTR(app_name_utf16.as_ptr());

    let spawn_res = unsafe {
        CreateProcessW(
            pcwstr_app_name,
            PWSTR(cmd_line_utf16.as_mut_ptr()),
            None,
            None,
            true, // bInheritHandles must be TRUE when using PROC_THREAD_ATTRIBUTE_HANDLE_LIST
            flags,
            Some(env_block.as_ptr() as *const _),
            pcwstr_work_dir,
            &si_ex.StartupInfo,
            &mut pi,
        )
    };

    // Clean up attribute list
    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }

    // Close the write end of pipes in the parent so reader threads receive EOF when child exits!
    drop(stdin_cleaner);
    drop(stdout_cleaner);
    drop(stderr_cleaner);

    if let Err(ref e) = spawn_res {
        tracing::debug!(
            "CreateProcessW error: {e}, app={:?}, work_dir={:?}, cmd={cmd_line_str}, has_ac={}",
            exe_path,
            spec.work_dir,
            app_container.is_some()
        );
    }
    spawn_res.map_err(|e| SandboxError::Internal(format!("CreateProcessW failed: {e}")))?;

    // Assign process to Job Object
    if let Err(e) = job.assign_process(pi.hProcess) {
        unsafe {
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
        return Err(SandboxError::Internal(format!(
            "AssignProcessToJobObject failed: {e}"
        )));
    }

    // Start background readers for stdout and stderr
    let stdout_limit = spec.limits.output_bytes as usize;
    let stderr_limit = spec.limits.stderr_bytes as usize;
    let ole_flag = Arc::new(AtomicBool::new(false));

    let stdout_handle = if let Some(r_handle) = stdout_read_handle {
        let ole_flag_clone = Arc::clone(&ole_flag);
        let raw_h = r_handle.0 as usize;
        Some(thread::spawn(move || {
            let handle = HANDLE(raw_h as *mut _);
            read_pipe_to_end(handle, stdout_limit, Some(ole_flag_clone))
        }))
    } else {
        None
    };

    let stderr_handle = if let Some(r_handle) = stderr_read_handle {
        let raw_h = r_handle.0 as usize;
        Some(thread::spawn(move || {
            let handle = HANDLE(raw_h as *mut _);
            read_pipe_to_end(handle, stderr_limit, None)
        }))
    } else {
        None
    };

    // Resume thread to begin execution and timing
    let start_time = Instant::now();
    unsafe {
        let _ = ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);
    }

    // Monitoring loop
    let mut killed_by: Option<KillReason> = None;
    let poll_interval = Duration::from_millis(5);

    loop {
        // 1. Cancellation check
        if cancel.is_cancelled() {
            killed_by = Some(KillReason::UserCancel);
            let _ = job.terminate(3);
            break;
        }

        // 2. Output limit check
        if ole_flag.load(Ordering::SeqCst) {
            killed_by = Some(KillReason::OutputLimit);
            let _ = job.terminate(3);
            break;
        }

        // 3. Wall clock check
        let wall_ms = start_time.elapsed().as_millis() as u64;
        if wall_ms > spec.limits.wall_time_ms {
            killed_by = Some(KillReason::WallLimit);
            let _ = job.terminate(3);
            break;
        }

        // 4. CPU time accounting check
        if let Ok(cpu_us) = job.query_cpu_time_us()
            && (cpu_us / 1000) > spec.limits.cpu_time_ms
        {
            killed_by = Some(KillReason::CpuLimit);
            let _ = job.terminate(3);
            break;
        }

        // 5. Memory check
        if let Ok(peak_mem) = job.query_peak_memory_bytes()
            && peak_mem > spec.limits.memory_bytes
        {
            killed_by = Some(KillReason::MemoryLimit);
            let _ = job.terminate(3);
            break;
        }

        // 6. Check if process terminated
        let wait_res = unsafe { WaitForSingleObject(pi.hProcess, 5) };
        if wait_res == WAIT_OBJECT_0 {
            break;
        }

        thread::sleep(poll_interval);
    }

    let wall_time_us = start_time.elapsed().as_micros() as u64;

    // Final wait to ensure all accounting is completed
    unsafe {
        let _ = WaitForSingleObject(pi.hProcess, 100);
    }

    let cpu_time_us = job.query_cpu_time_us().unwrap_or(0);
    let memory_peak_bytes = job.query_peak_memory_bytes().unwrap_or(0);

    let mut exit_code: u32 = 0;
    unsafe {
        let _ = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        let _ = CloseHandle(pi.hProcess);
    }

    // Join readers
    let captured_stdout = if let Some(h) = stdout_handle {
        let bytes = h.join().unwrap_or_default();
        let truncated = ole_flag.load(Ordering::SeqCst);
        Some(CapturedOutput { bytes, truncated })
    } else {
        None
    };

    if captured_stdout.as_ref().is_some_and(|o| o.truncated) && killed_by.is_none() {
        killed_by = Some(KillReason::OutputLimit);
    }

    let stderr_tail = if let Some(h) = stderr_handle {
        h.join().unwrap_or_default()
    } else {
        Vec::new()
    };

    let status = if is_ntstatus_exception(exit_code) {
        ExitStatus::NtStatus(exit_code)
    } else {
        ExitStatus::Exited(exit_code as i32)
    };

    let oom_killed = killed_by == Some(KillReason::MemoryLimit);

    Ok(RunResult {
        status,
        cpu_time_us,
        wall_time_us,
        memory_peak_bytes,
        killed_by,
        oom_killed,
        stdout: captured_stdout,
        stderr_tail,
        residue_cleaned: false,
        sandbox_tier: tier,
    })
}

// ----------------------------------------------------------------------------
// Helpers: Pipe and Environment preparation
// ----------------------------------------------------------------------------

struct HandleCleaner {
    handle: HANDLE,
}

impl Drop for HandleCleaner {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

fn prepare_stdin(stdin: &Stdio) -> Result<(HANDLE, Option<HandleCleaner>), SandboxError> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
    };

    match stdin {
        Stdio::Null => {
            let nul_utf16: Vec<u16> = "NUL\0".encode_utf16().collect();
            let h = unsafe {
                CreateFileW(
                    PCWSTR(nul_utf16.as_ptr()),
                    FILE_GENERIC_READ.0,
                    FILE_SHARE_READ,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )?
            };
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((h, Some(HandleCleaner { handle: h })))
        }
        Stdio::File(path) => {
            let path_utf16: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let h = unsafe {
                CreateFileW(
                    PCWSTR(path_utf16.as_ptr()),
                    FILE_GENERIC_READ.0,
                    FILE_SHARE_READ,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )?
            };
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((h, Some(HandleCleaner { handle: h })))
        }
        Stdio::Pipe | Stdio::Inherit => {
            // Default to null if not wired
            prepare_stdin(&Stdio::Null)
        }
    }
}

fn prepare_stdout(
    stdout: &Stdio,
) -> Result<(HANDLE, Option<HANDLE>, Option<HandleCleaner>), SandboxError> {
    use windows::Win32::System::Pipes::CreatePipe;

    match stdout {
        Stdio::Pipe => {
            let mut read_pipe = HANDLE::default();
            let mut write_pipe = HANDLE::default();
            unsafe {
                CreatePipe(&mut read_pipe, &mut write_pipe, None, 0)?;
                let _ =
                    SetHandleInformation(write_pipe, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((
                write_pipe,
                Some(read_pipe),
                Some(HandleCleaner { handle: write_pipe }),
            ))
        }
        Stdio::File(path) => {
            use windows::Win32::Storage::FileSystem::{
                CREATE_ALWAYS, CreateFileW, FILE_GENERIC_WRITE, FILE_SHARE_READ,
            };
            let path_utf16: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let h = unsafe {
                CreateFileW(
                    PCWSTR(path_utf16.as_ptr()),
                    FILE_GENERIC_WRITE.0,
                    FILE_SHARE_READ,
                    None,
                    CREATE_ALWAYS,
                    Default::default(),
                    None,
                )?
            };
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((h, None, Some(HandleCleaner { handle: h })))
        }
        Stdio::Null | Stdio::Inherit => {
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_EXISTING,
            };
            let nul_utf16: Vec<u16> = "NUL\0".encode_utf16().collect();
            let h = unsafe {
                CreateFileW(
                    PCWSTR(nul_utf16.as_ptr()),
                    FILE_GENERIC_WRITE.0,
                    FILE_SHARE_READ,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )?
            };
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((h, None, Some(HandleCleaner { handle: h })))
        }
    }
}

fn prepare_stderr(
    _stderr: &Stdio,
) -> Result<(HANDLE, Option<HANDLE>, Option<HandleCleaner>), SandboxError> {
    // Always create a pipe to capture stderr for diagnostics
    prepare_stdout(&Stdio::Pipe)
}

fn read_pipe_to_end(
    pipe_handle: HANDLE,
    limit: usize,
    ole_flag: Option<Arc<AtomicBool>>,
) -> Vec<u8> {
    use windows::Win32::Storage::FileSystem::ReadFile;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut bytes_read: u32 = 0;

    unsafe {
        while ReadFile(pipe_handle, Some(&mut chunk), Some(&mut bytes_read), None).is_ok() {
            if bytes_read == 0 {
                break;
            }
            let n = bytes_read as usize;
            if buf.len() + n > limit {
                if let Some(flag) = &ole_flag {
                    flag.store(true, Ordering::SeqCst);
                }
                let keep = limit.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..keep]);
                break;
            } else {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = CloseHandle(pipe_handle);
    }
    buf
}

fn build_windows_cmd_line(argv: &[std::ffi::OsString]) -> String {
    let mut line = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let s = arg.to_string_lossy();
        if s.contains(' ') || s.contains('\t') || s.is_empty() {
            line.push('"');
            line.push_str(&s.replace('"', "\\\""));
            line.push('"');
        } else {
            line.push_str(&s);
        }
    }
    line
}

fn build_environment_block(
    env: &[(std::ffi::OsString, std::ffi::OsString)],
    work_dir: &Path,
) -> Vec<u16> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, String> = BTreeMap::new();

    // Preserve system essentials for Windows runtime and DLL initialization
    for (k, v) in std::env::vars() {
        let upper = k.to_uppercase();
        if upper == "SYSTEMROOT"
            || upper == "WINDIR"
            || upper == "PATH"
            || upper == "PATHEXT"
            || upper == "SYSTEMDRIVE"
            || upper == "COMSPEC"
            || upper == "ALLUSERSPROFILE"
            || upper == "PROGRAMDATA"
            || upper == "PROGRAMFILES"
            || upper == "PROGRAMFILES(X86)"
            || upper == "COMMONPROGRAMFILES"
            || upper == "APPDATA"
            || upper == "LOCALAPPDATA"
            || upper == "USERPROFILE"
            || upper == "HOMEDRIVE"
            || upper == "HOMEPATH"
        {
            map.insert(k, v);
        }
    }

    map.insert("TEMP".to_string(), work_dir.to_string_lossy().to_string());
    map.insert("TMP".to_string(), work_dir.to_string_lossy().to_string());

    for (k, v) in env {
        map.insert(
            k.to_string_lossy().to_string(),
            v.to_string_lossy().to_string(),
        );
    }

    let mut block = Vec::new();
    for (k, v) in map {
        for ch in format!("{k}={v}\0").encode_utf16() {
            block.push(ch);
        }
    }

    // Double null termination
    block.push(0);
    block
}

//! Process creation and execution monitoring for Windows sandbox.

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::Security::SECURITY_CAPABILITIES;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    GetProcessTimes, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_core::{PCWSTR, PWSTR};

use crate::ntstatus::is_ntstatus_exception;
use crate::windows::job::JobObject;
use crate::windows::security::AppContainerProfile;
use crate::{CancelToken, SandboxError, Wiring};

// Thread attribute keys:
pub const ATTR_HANDLE_LIST: usize = 0x00020002;
pub const ATTR_SECURITY_CAPABILITIES: usize = 0x00020009;
pub const ATTR_CHILD_PROCESS_POLICY: usize = 0x0002000E;
pub const PROCESS_CREATION_CHILD_PROCESS_RESTRICTED: u32 = 0x01;

/// Upper bound of a monitor wait slice: limits (CPU / wall / output / cancel) are re-checked
/// at least this often while the monitored processes are running.
const MONITOR_SLICE_MS: u64 = 5;

/// `MAXIMUM_WAIT_OBJECTS` for `WaitForMultipleObjects`.
const MAX_WAIT_OBJECTS: usize = 64;

/// `memory_bytes == 0` means "unlimited" (consistent with the Linux backend and the job
/// limit setup, which only installs a memory cap when the limit is non-zero).
#[inline]
pub(crate) fn exceeds_memory_limit(peak_bytes: u64, limit_bytes: u64) -> bool {
    limit_bytes > 0 && peak_bytes > limit_bytes
}

/// Wait slice for the monitor loops: `min(MONITOR_SLICE_MS, remaining wall + 1)`, at least 1 ms.
#[inline]
fn monitor_slice_ms(remaining_wall_ms: u64) -> u32 {
    remaining_wall_ms
        .saturating_add(1)
        .clamp(1, MONITOR_SLICE_MS) as u32
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Current system time as a FILETIME value (100 ns units).
fn precise_now_filetime() -> u64 {
    filetime_to_u64(unsafe { GetSystemTimePreciseAsFileTime() })
}

/// Exit time of a terminated process as a FILETIME value (100 ns units).
fn process_exit_filetime(h_process: HANDLE) -> Option<u64> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(h_process, &mut creation, &mut exit, &mut kernel, &mut user) }
        .ok()
        .map(|_| filetime_to_u64(exit))
        .filter(|&t| t != 0)
}

/// Resolve `argv[0]` to an executable path (PATH / PATHEXT search via `which`).
///
/// Bare command names (no path separator, e.g. `cmd.exe`, `python`) are resolved once per
/// process and memoized: the PATH walk costs several filesystem probes per spawn. Explicit
/// paths are not cached because the file may be (re)created between runs.
pub(crate) fn resolve_executable(arg0: &std::ffi::OsStr) -> std::path::PathBuf {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::LazyLock;

    static WHICH_CACHE: LazyLock<parking_lot::Mutex<HashMap<OsString, PathBuf>>> =
        LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

    let s = arg0.to_string_lossy();
    let is_bare_name = !s.is_empty() && !s.contains(['/', '\\', ':']);
    if is_bare_name && let Some(p) = WHICH_CACHE.lock().get(arg0) {
        return p.clone();
    }
    match which::which(arg0) {
        Ok(p) => {
            if is_bare_name {
                WHICH_CACHE.lock().insert(arg0.to_os_string(), p.clone());
            }
            p
        }
        Err(_) => PathBuf::from(arg0),
    }
}

/// Apply the process-wide error mode once (inherited by every spawned child): never show
/// critical-error / GP-fault / open-file dialog boxes that would hang an unattended judge.
///
/// `SetErrorMode` is process-global state, so setting it on every spawn was redundant.
fn ensure_process_error_mode() {
    use windows::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    };
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    });
}

pub(crate) unsafe fn spawn_suspended_process(
    spec: &RunSpec,
    h_stdin: HANDLE,
    h_stdout: HANDLE,
    h_stderr: HANDLE,
    app_container: Option<&AppContainerProfile>,
) -> Result<PROCESS_INFORMATION, SandboxError> {
    if spec.argv.is_empty() {
        return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
    }

    let exe_path = resolve_executable(&spec.argv[0]);
    let mut full_argv = spec.argv.clone();
    full_argv[0] = exe_path.clone().into_os_string();

    let cmd_line_str = build_windows_cmd_line(&full_argv);
    let mut cmd_line_utf16: Vec<u16> = cmd_line_str.encode_utf16().chain(Some(0)).collect();

    let work_dir_utf16: Vec<u16> = spec
        .work_dir
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let pcwstr_work_dir = PCWSTR(work_dir_utf16.as_ptr());

    let mut inherit_handles = vec![h_stdin, h_stdout, h_stderr];
    for extra in &spec.extra_fds {
        let h = HANDLE(extra.0 as *mut _);
        if !h.is_invalid() && !inherit_handles.contains(&h) {
            inherit_handles.push(h);
        }
    }

    let mut attr_count = 1;
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

    let env_block = build_environment_block(&spec.env, &spec.work_dir);
    let mut pi = PROCESS_INFORMATION::default();

    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT
        | CREATE_NO_WINDOW
        | CREATE_SUSPENDED;

    let app_name_utf16: Vec<u16> = exe_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let pcwstr_app_name = PCWSTR(app_name_utf16.as_ptr());

    ensure_process_error_mode();

    let spawn_res = unsafe {
        CreateProcessW(
            pcwstr_app_name,
            PWSTR(cmd_line_utf16.as_mut_ptr()),
            None,
            None,
            true,
            flags,
            Some(env_block.as_ptr() as *const _),
            pcwstr_work_dir,
            &si_ex.StartupInfo,
            &mut pi,
        )
    };

    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }

    if let Err(ref e) = spawn_res {
        tracing::debug!(
            "CreateProcessW error: {e}, app={:?}, work_dir={:?}, cmd={cmd_line_str}",
            exe_path,
            spec.work_dir
        );
    }
    spawn_res.map_err(|e| SandboxError::Internal(format!("CreateProcessW failed: {e}")))?;

    Ok(pi)
}

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

    // Prepare pipes / file handles for stdin, stdout, stderr
    let (h_stdin, stdin_cleaner) = prepare_stdin(&spec.stdin)?;
    let (h_stdout, stdout_read_cleaner, stdout_cleaner) = prepare_stdout(&spec.stdout)?;
    let (h_stderr, stderr_read_cleaner, stderr_cleaner) = prepare_stderr(&spec.stderr)?;

    let pi = unsafe { spawn_suspended_process(spec, h_stdin, h_stdout, h_stderr, app_container)? };

    // Close the write end of pipes in the parent so reader threads receive EOF when child exits!
    drop(stdin_cleaner);
    drop(stdout_cleaner);
    drop(stderr_cleaner);

    // Assign process to Job Object
    if let Err(e) = job.assign_process(pi.hProcess) {
        unsafe {
            let _ = TerminateProcess(pi.hProcess, 1);
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

    let stdout_handle = if let Some(r_cleaner) = stdout_read_cleaner {
        let r_handle = r_cleaner.into_raw();
        let ole_flag_clone = Arc::clone(&ole_flag);
        let raw_h = r_handle.0 as usize;
        Some(thread::spawn(move || {
            let handle = HANDLE(raw_h as *mut _);
            read_pipe_to_end(handle, stdout_limit, Some(ole_flag_clone))
        }))
    } else {
        None
    };

    let stderr_handle = if let Some(r_cleaner) = stderr_read_cleaner {
        let r_handle = r_cleaner.into_raw();
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

    // Monitoring loop (event-driven).
    //
    // The thread blocks on the job's I/O completion port with a short timeout slice, so
    // EXIT_PROCESS / ACTIVE_PROCESS_ZERO / memory-limit notifications wake it immediately,
    // while CPU / wall / output / cancellation limits are still re-checked at least every
    // `MONITOR_SLICE_MS`. Job notifications are not guaranteed to be delivered, so process
    // and job state is always re-queried after each wake-up; the slice timeout bounds the
    // detection latency in that case. The loop never busy-spins (fixes the 100% CPU spin
    // when the main process exited while descendants were still alive).
    let mut killed_by: Option<KillReason> = None;
    let mut main_exited = false;

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

        // 5. Check if process terminated (and, if descendants are allowed, whether the
        //    whole job drained).
        if !main_exited && unsafe { WaitForSingleObject(pi.hProcess, 0) } == WAIT_OBJECT_0 {
            main_exited = true;
        }
        if main_exited
            && (spec.limits.max_processes <= 1
                || job.active_process_zero_seen()
                || job.query_active_processes().unwrap_or(0) == 0)
        {
            break;
        }

        // 6. Memory check (memory_bytes == 0 means unlimited)
        if job.check_memory_limit_violation() {
            killed_by = Some(KillReason::MemoryLimit);
            let _ = job.terminate(3);
            break;
        }

        if let Ok(peak_mem) = job.query_peak_memory_bytes()
            && exceeds_memory_limit(peak_mem, spec.limits.memory_bytes)
        {
            killed_by = Some(KillReason::MemoryLimit);
            let _ = job.terminate(3);
            break;
        }

        // 7. Block until the next job notification or the slice elapses.
        let remaining_wall = spec.limits.wall_time_ms.saturating_sub(wall_ms);
        let _ = job.wait_message(monitor_slice_ms(remaining_wall));
    }

    let wall_time_us = start_time.elapsed().as_micros() as u64;

    // Final wait to ensure all accounting is completed
    unsafe {
        let _ = WaitForSingleObject(pi.hProcess, 100);
    }

    let cpu_time_us = job.query_cpu_time_us().unwrap_or(0);
    let memory_peak_bytes = job.query_peak_memory_bytes().unwrap_or(0);
    let mem_violated = job.check_memory_limit_violation();
    if mem_violated && killed_by.is_none() {
        killed_by = Some(KillReason::MemoryLimit);
    }

    let mut exit_code: u32 = 0;
    unsafe {
        let _ = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        let _ = CloseHandle(pi.hProcess);
    }

    // CRITICAL (P0-02): Terminate any remaining descendant processes in the Job.
    // Child processes that inherited stdout/stderr hold write pipe handles open.
    // If not terminated, reader threads will block in ReadFile indefinitely!
    let _ = job.terminate(3);

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

    let oom_killed = killed_by == Some(KillReason::MemoryLimit)
        || mem_violated
        || exceeds_memory_limit(memory_peak_bytes, spec.limits.memory_bytes);

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
        exit_order: None,
    })
}

pub fn execute_group_in_jobs(
    specs: &[RunSpec],
    wiring: &Wiring,
    app_container: Option<&AppContainerProfile>,
    cancel: &CancelToken,
    tier: Tier,
) -> Result<Vec<RunResult>, SandboxError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }

    let n = specs.len();
    let mut specs = specs.to_vec();

    // 1. Create wired pipes
    let mut wired_stdins: Vec<Option<HANDLE>> = vec![None; n];
    let mut wired_stdouts: Vec<Option<HANDLE>> = vec![None; n];
    let mut pipe_cleaners: Vec<HandleCleaner> = Vec::new();

    for conn in &wiring.connections {
        if conn.from_proc >= n || conn.to_proc >= n {
            return Err(SandboxError::Internal(format!(
                "Invalid wiring connection from {} to {} (total procs: {})",
                conn.from_proc, conn.to_proc, n
            )));
        }
        if wired_stdouts[conn.from_proc].is_some() || wired_stdins[conn.to_proc].is_some() {
            return Err(SandboxError::Internal(format!(
                "Duplicate stdio wiring connection from {} to {}",
                conn.from_proc, conn.to_proc
            )));
        }
        use windows::Win32::System::Pipes::CreatePipe;
        let mut h_read = HANDLE::default();
        let mut h_write = HANDLE::default();
        unsafe {
            CreatePipe(&mut h_read, &mut h_write, None, 0)
                .map_err(|e| SandboxError::Internal(format!("CreatePipe failed: {e}")))?;
            let _ = SetHandleInformation(h_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            let _ = SetHandleInformation(h_write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
        }
        wired_stdouts[conn.from_proc] = Some(h_write);
        wired_stdins[conn.to_proc] = Some(h_read);
        pipe_cleaners.push(HandleCleaner { handle: h_read });
        pipe_cleaners.push(HandleCleaner { handle: h_write });
    }

    if let Some(mgr_idx) = wiring.star_manager_proc {
        if mgr_idx >= n {
            return Err(SandboxError::Internal(format!(
                "Invalid star_manager_proc {} (total procs: {})",
                mgr_idx, n
            )));
        }
        use windows::Win32::System::Pipes::CreatePipe;
        let mut star_fds_env = Vec::new();
        for p in 0..n {
            if p == mgr_idx {
                continue;
            }
            if wired_stdins[p].is_some() || wired_stdouts[p].is_some() {
                return Err(SandboxError::Internal(format!(
                    "Duplicate stdio wiring for star participant {p}"
                )));
            }
            let mut p_to_m_read = HANDLE::default();
            let mut p_to_m_write = HANDLE::default();
            unsafe {
                CreatePipe(&mut p_to_m_read, &mut p_to_m_write, None, 65536)
                    .map_err(|e| SandboxError::Internal(format!("CreatePipe failed: {e}")))?;
                let _ =
                    SetHandleInformation(p_to_m_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
                let _ =
                    SetHandleInformation(p_to_m_write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            pipe_cleaners.push(HandleCleaner {
                handle: p_to_m_read,
            });
            pipe_cleaners.push(HandleCleaner {
                handle: p_to_m_write,
            });

            let mut m_to_p_read = HANDLE::default();
            let mut m_to_p_write = HANDLE::default();
            unsafe {
                CreatePipe(&mut m_to_p_read, &mut m_to_p_write, None, 65536)
                    .map_err(|e| SandboxError::Internal(format!("CreatePipe failed: {e}")))?;
                let _ =
                    SetHandleInformation(m_to_p_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
                let _ =
                    SetHandleInformation(m_to_p_write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            pipe_cleaners.push(HandleCleaner {
                handle: m_to_p_read,
            });
            pipe_cleaners.push(HandleCleaner {
                handle: m_to_p_write,
            });

            wired_stdouts[p] = Some(p_to_m_write);
            wired_stdins[p] = Some(m_to_p_read);

            let r_val = p_to_m_read.0 as usize;
            let w_val = m_to_p_write.0 as usize;
            specs[mgr_idx]
                .extra_fds
                .push(uj_core::run::RawFdOrHandle(r_val));
            specs[mgr_idx]
                .extra_fds
                .push(uj_core::run::RawFdOrHandle(w_val));
            specs[mgr_idx]
                .argv
                .push(std::ffi::OsString::from(r_val.to_string()));
            specs[mgr_idx]
                .argv
                .push(std::ffi::OsString::from(w_val.to_string()));
            star_fds_env.push(format!("{r_val},{w_val}"));
        }
        specs[mgr_idx].env.push((
            std::ffi::OsString::from("UJ_STAR_FDS"),
            std::ffi::OsString::from(star_fds_env.join(";")),
        ));
    }

    // 2. Prepare I/O for each process
    let mut stdin_handles = Vec::with_capacity(n);
    let mut stdout_handles = Vec::with_capacity(n);
    let mut stderr_handles = Vec::with_capacity(n);
    let mut stdout_read_cleaners = Vec::with_capacity(n);
    let mut stderr_read_cleaners = Vec::with_capacity(n);
    let mut extra_cleaners = Vec::new();

    for i in 0..n {
        let (h_in, in_cleaner) = if let Some(h) = wired_stdins[i] {
            (h, None)
        } else {
            prepare_stdin(&specs[i].stdin)?
        };
        stdin_handles.push(h_in);
        if let Some(c) = in_cleaner {
            extra_cleaners.push(c);
        }

        let (h_out, r_out, out_cleaner) = if let Some(h) = wired_stdouts[i] {
            (h, None, None)
        } else {
            prepare_stdout(&specs[i].stdout)?
        };
        stdout_handles.push(h_out);
        stdout_read_cleaners.push(r_out);
        if let Some(c) = out_cleaner {
            extra_cleaners.push(c);
        }

        let (h_err, r_err, err_cleaner) = prepare_stderr(&specs[i].stderr)?;
        stderr_handles.push(h_err);
        stderr_read_cleaners.push(r_err);
        if let Some(c) = err_cleaner {
            extra_cleaners.push(c);
        }
    }

    struct ScopedGroupProcs(Vec<PROCESS_INFORMATION>);
    impl Drop for ScopedGroupProcs {
        fn drop(&mut self) {
            for pi in self.0.drain(..) {
                unsafe {
                    let _ = TerminateProcess(pi.hProcess, 1);
                    let _ = CloseHandle(pi.hThread);
                    let _ = CloseHandle(pi.hProcess);
                }
            }
        }
    }

    // 3. Create Job Objects and Spawn suspended processes
    let mut jobs = Vec::with_capacity(n);
    let mut scoped_procs = ScopedGroupProcs(Vec::with_capacity(n));

    for i in 0..n {
        let job = JobObject::create(None).map_err(|e| {
            SandboxError::Internal(format!("Failed to create Job Object for proc {i}: {e}"))
        })?;
        job.set_limits(
            specs[i].limits.memory_bytes,
            specs[i].limits.max_processes,
            specs[i].cpu_affinity.as_deref(),
        )
        .map_err(|e| SandboxError::Internal(format!("Failed to set limits for proc {i}: {e}")))?;

        let ac = if specs[i].trusted {
            None
        } else {
            app_container
        };
        let pi = unsafe {
            spawn_suspended_process(
                &specs[i],
                stdin_handles[i],
                stdout_handles[i],
                stderr_handles[i],
                ac,
            )?
        };

        if let Err(e) = job.assign_process(pi.hProcess) {
            unsafe {
                let _ = TerminateProcess(pi.hProcess, 1);
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
            }
            return Err(SandboxError::Internal(format!(
                "AssignProcessToJobObject for proc {i} failed: {e}"
            )));
        }

        jobs.push(job);
        scoped_procs.0.push(pi);
    }

    let proc_infos = std::mem::take(&mut scoped_procs.0);

    // 4. IMPORTANT: Close all parent copies of pipe handles so children receive EOF!
    drop(pipe_cleaners);
    drop(extra_cleaners);

    // 5. Start background readers
    let mut ole_flags = Vec::with_capacity(n);
    let mut stdout_threads = Vec::with_capacity(n);
    let mut stderr_threads = Vec::with_capacity(n);

    for i in 0..n {
        let ole_flag = Arc::new(AtomicBool::new(false));
        ole_flags.push(ole_flag.clone());

        let stdout_limit = specs[i].limits.output_bytes as usize;
        let s_thread = if let Some(r_cleaner) = stdout_read_cleaners[i].take() {
            let r_handle = r_cleaner.into_raw();
            let flag_clone = ole_flag.clone();
            let raw_h = r_handle.0 as usize;
            Some(thread::spawn(move || {
                let handle = HANDLE(raw_h as *mut _);
                read_pipe_to_end(handle, stdout_limit, Some(flag_clone))
            }))
        } else {
            None
        };
        stdout_threads.push(s_thread);

        let stderr_limit = specs[i].limits.stderr_bytes as usize;
        let e_thread = if let Some(r_cleaner) = stderr_read_cleaners[i].take() {
            let r_handle = r_cleaner.into_raw();
            let raw_h = r_handle.0 as usize;
            Some(thread::spawn(move || {
                let handle = HANDLE(raw_h as *mut _);
                read_pipe_to_end(handle, stderr_limit, None)
            }))
        } else {
            None
        };
        stderr_threads.push(e_thread);
    }

    // 6. Resume all threads simultaneously
    let start_time = Instant::now();
    for pi in &proc_infos {
        unsafe {
            let _ = ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
        }
    }

    // 7. Group monitoring loop
    let mut finished = vec![false; n];
    let mut killed_by: Vec<Option<KillReason>> = vec![None; n];
    let mut exit_order: Vec<Option<u32>> = vec![None; n];
    let mut order_counter = 1u32;
    let mut exit_statuses = vec![ExitStatus::Exited(0); n];
    let mut final_cpu_us = vec![0u64; n];
    let mut final_wall_us = vec![0u64; n];
    let mut final_peak_mem = vec![0u64; n];

    let group_wall_limit = specs
        .iter()
        .map(|s| s.limits.wall_time_ms)
        .max()
        .unwrap_or(10000);
    // Event-driven group monitoring: block in `WaitForMultipleObjects` on the handles of all
    // still-running processes with a short timeout slice (for CPU / wall / output / cancel
    // checks). A process exit wakes the loop immediately so `final_wall_us` and `exit_order`
    // are recorded at the actual exit time instead of being rounded to a 5 ms poll.
    loop {
        if finished.iter().all(|&f| f) {
            break;
        }

        // 1. Cancellation check
        if cancel.is_cancelled() {
            for i in 0..n {
                if !finished[i] {
                    killed_by[i] = Some(KillReason::UserCancel);
                    let _ = jobs[i].terminate(3);
                }
            }
            break;
        }

        // 2. Wall clock check across group
        let wall_ms = start_time.elapsed().as_millis() as u64;
        if wall_ms > group_wall_limit {
            for i in 0..n {
                if !finished[i] {
                    killed_by[i] = Some(KillReason::WallLimit);
                    let _ = jobs[i].terminate(3);
                }
            }
            break;
        }

        // 3. Check each process. Events detected in the same pass are ordered by the time
        //    they actually happened (process exit time / kill decision time).
        let mut pass_events: Vec<(u64, usize)> = Vec::new();
        for i in 0..n {
            if finished[i] {
                continue;
            }

            // OLE check
            if ole_flags[i].load(Ordering::SeqCst) {
                killed_by[i] = Some(KillReason::OutputLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;
                pass_events.push((precise_now_filetime(), i));
                continue;
            }

            // CPU check
            if let Ok(cpu_us) = jobs[i].query_cpu_time_us()
                && (cpu_us / 1000) > specs[i].limits.cpu_time_ms
            {
                killed_by[i] = Some(KillReason::CpuLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;
                pass_events.push((precise_now_filetime(), i));
                continue;
            }

            // Process exit check
            let wait_res = unsafe { WaitForSingleObject(proc_infos[i].hProcess, 0) };
            if wait_res == WAIT_OBJECT_0 {
                finished[i] = true;
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;

                let mut exit_code: u32 = 0;
                unsafe {
                    let _ = GetExitCodeProcess(proc_infos[i].hProcess, &mut exit_code);
                }
                exit_statuses[i] = if is_ntstatus_exception(exit_code) {
                    ExitStatus::NtStatus(exit_code)
                } else {
                    ExitStatus::Exited(exit_code as i32)
                };
                final_cpu_us[i] = jobs[i].query_cpu_time_us().unwrap_or(0);
                final_peak_mem[i] = jobs[i].query_peak_memory_bytes().unwrap_or(0);
                let exit_ft = process_exit_filetime(proc_infos[i].hProcess)
                    .unwrap_or_else(precise_now_filetime);
                pass_events.push((exit_ft, i));
                continue;
            }

            // Memory check (memory_bytes == 0 means unlimited)
            if jobs[i].check_memory_limit_violation() {
                killed_by[i] = Some(KillReason::MemoryLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;
                pass_events.push((precise_now_filetime(), i));
                continue;
            }

            if let Ok(mem) = jobs[i].query_peak_memory_bytes()
                && exceeds_memory_limit(mem, specs[i].limits.memory_bytes)
            {
                killed_by[i] = Some(KillReason::MemoryLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;
                pass_events.push((precise_now_filetime(), i));
                continue;
            }
        }

        // Stable sort keeps index order for events with identical timestamps.
        pass_events.sort_by_key(|&(t, _)| t);
        for (_, i) in pass_events {
            exit_order[i] = Some(order_counter);
            order_counter += 1;
        }

        // 4. Block until any running process exits or the slice elapses.
        let waitable: Vec<HANDLE> = (0..n)
            .filter(|&i| !finished[i])
            .map(|i| proc_infos[i].hProcess)
            .take(MAX_WAIT_OBJECTS)
            .collect();
        if waitable.is_empty() {
            continue;
        }
        let remaining_wall =
            group_wall_limit.saturating_sub(start_time.elapsed().as_millis() as u64);
        unsafe {
            let _ = WaitForMultipleObjects(&waitable, false, monitor_slice_ms(remaining_wall));
        }
    }

    // Wall time for processes still running when the loop ended (cancel / group wall limit),
    // captured before the per-process termination waits below can skew it.
    let loop_end_wall_us = start_time.elapsed().as_micros() as u64;

    // 8. Collect results for all processes
    let mut results = Vec::with_capacity(n);
    for i in 0..n {
        unsafe {
            let _ = WaitForSingleObject(proc_infos[i].hProcess, 100);
        }

        let mem_violated = jobs[i].check_memory_limit_violation();
        if mem_violated && killed_by[i].is_none() {
            killed_by[i] = Some(KillReason::MemoryLimit);
        }

        if finished[i] && killed_by[i].is_some() && final_cpu_us[i] == 0 {
            // Killed inside the loop (OLE / CPU / memory): report the real accounting
            // instead of zeros.
            final_cpu_us[i] = jobs[i].query_cpu_time_us().unwrap_or(0);
            final_peak_mem[i] = jobs[i].query_peak_memory_bytes().unwrap_or(0);
        }

        if !finished[i] {
            let mut exit_code: u32 = 0;
            unsafe {
                let _ = GetExitCodeProcess(proc_infos[i].hProcess, &mut exit_code);
            }
            exit_statuses[i] = if is_ntstatus_exception(exit_code) {
                ExitStatus::NtStatus(exit_code)
            } else {
                ExitStatus::Exited(exit_code as i32)
            };
            final_cpu_us[i] = jobs[i].query_cpu_time_us().unwrap_or(0);
            final_peak_mem[i] = jobs[i].query_peak_memory_bytes().unwrap_or(0);
            final_wall_us[i] = loop_end_wall_us;
            exit_order[i] = Some(order_counter);
            order_counter += 1;
        }

        unsafe {
            let _ = CloseHandle(proc_infos[i].hProcess);
        }

        let captured_stdout = if let Some(Some(h)) = stdout_threads.get_mut(i).map(|t| t.take()) {
            let bytes = h.join().unwrap_or_default();
            let truncated = ole_flags[i].load(Ordering::SeqCst);
            Some(CapturedOutput { bytes, truncated })
        } else {
            None
        };

        let stderr_tail = if let Some(Some(h)) = stderr_threads.get_mut(i).map(|t| t.take()) {
            h.join().unwrap_or_default()
        } else {
            Vec::new()
        };

        let oom_killed = killed_by[i] == Some(KillReason::MemoryLimit)
            || mem_violated
            || exceeds_memory_limit(final_peak_mem[i], specs[i].limits.memory_bytes);

        results.push(RunResult {
            status: exit_statuses[i],
            cpu_time_us: final_cpu_us[i],
            wall_time_us: final_wall_us[i],
            memory_peak_bytes: final_peak_mem[i],
            killed_by: killed_by[i],
            oom_killed,
            stdout: captured_stdout,
            stderr_tail,
            residue_cleaned: false,
            sandbox_tier: tier,
            exit_order: exit_order[i],
        });
    }

    Ok(results)
}

// ----------------------------------------------------------------------------
// Helpers: Pipe and Environment preparation
// ----------------------------------------------------------------------------

struct HandleCleaner {
    handle: HANDLE,
}

impl HandleCleaner {
    fn into_raw(mut self) -> HANDLE {
        let h = self.handle;
        self.handle = HANDLE::default();
        h
    }
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
) -> Result<(HANDLE, Option<HandleCleaner>, Option<HandleCleaner>), SandboxError> {
    use windows::Win32::System::Pipes::CreatePipe;

    match stdout {
        Stdio::Pipe => {
            let mut read_pipe = HANDLE::default();
            let mut write_pipe = HANDLE::default();
            unsafe {
                CreatePipe(&mut read_pipe, &mut write_pipe, None, 65536)?;
                let _ =
                    SetHandleInformation(write_pipe, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
            }
            Ok((
                write_pipe,
                Some(HandleCleaner { handle: read_pipe }),
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
) -> Result<(HANDLE, Option<HandleCleaner>, Option<HandleCleaner>), SandboxError> {
    // Always create a pipe to capture stderr for diagnostics
    prepare_stdout(&Stdio::Pipe)
}

fn read_pipe_to_end(
    pipe_handle: HANDLE,
    limit: usize,
    ole_flag: Option<Arc<AtomicBool>>,
) -> Vec<u8> {
    use windows::Win32::Storage::FileSystem::ReadFile;

    let mut buf = Vec::with_capacity(limit.min(65536));
    let mut chunk = vec![0u8; 65536];
    let mut bytes_read: u32 = 0;

    unsafe {
        while ReadFile(
            pipe_handle,
            Some(chunk.as_mut_slice()),
            Some(&mut bytes_read),
            None,
        )
        .is_ok()
        {
            if bytes_read == 0 {
                break;
            }
            let n = bytes_read as usize;
            if buf.len() < limit {
                let keep = n.min(limit - buf.len());
                buf.extend_from_slice(&chunk[..keep]);
                if buf.len() >= limit && ole_flag.is_some() {
                    if let Some(flag) = &ole_flag {
                        flag.store(true, Ordering::SeqCst);
                    }
                    break;
                }
            } else if ole_flag.is_some() {
                if let Some(flag) = &ole_flag {
                    flag.store(true, Ordering::SeqCst);
                }
                break;
            }
            // For stderr (ole_flag is None): keep draining into chunk so the child process
            // does not receive a broken pipe error when stderr is chatty.
        }
        let _ = CloseHandle(pipe_handle);
    }
    buf
}

/// Build a Windows command line string adhering to the MSVC CRT / `CommandLineToArgvW`
/// escaping rules:
/// - Quote arguments that are empty or contain spaces, tabs, newlines, vertical tabs, or `"`.
/// - Inside quotes, double any backslashes immediately preceding `"` and escape `"` as `\"`.
/// - Double any trailing backslashes immediately preceding the closing `"`.
pub(crate) fn build_windows_cmd_line(argv: &[std::ffi::OsString]) -> String {
    let mut line = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let s = arg.to_string_lossy();
        let needs_quotes = s.is_empty()
            || s.chars()
                .any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\x0b' || c == '"');
        if !needs_quotes {
            line.push_str(&s);
            continue;
        }
        line.push('"');
        let mut backslashes = 0usize;
        for c in s.chars() {
            if c == '\\' {
                backslashes += 1;
            } else if c == '"' {
                for _ in 0..(backslashes * 2 + 1) {
                    line.push('\\');
                }
                line.push('"');
                backslashes = 0;
            } else {
                for _ in 0..backslashes {
                    line.push('\\');
                }
                line.push(c);
                backslashes = 0;
            }
        }
        for _ in 0..(backslashes * 2) {
            line.push('\\');
        }
        line.push('"');
    }
    line
}

/// System essentials of the parent environment preserved for Windows runtime and DLL
/// initialization. Snapshotted once per process (the judge never mutates these variables
/// at runtime), so each spawn no longer walks and upper-cases the full parent environment.
fn base_environment() -> &'static std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    static BASE_ENV: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
    BASE_ENV.get_or_init(|| {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
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
        map
    })
}

fn build_environment_block(
    env: &[(std::ffi::OsString, std::ffi::OsString)],
    work_dir: &Path,
) -> Vec<u16> {
    // Preserve system essentials for Windows runtime and DLL initialization
    let mut map = base_environment().clone();

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

/// Pre-warm newly prepared binary to trigger Windows Defender (`WdFilter.sys`) synchronous image
/// section scan ahead of timed execution.
/// Spawns the process with `CREATE_SUSPENDED | CREATE_NO_WINDOW` (which maps the image section
/// and completes Defender's synchronous kernel callback inside `NtCreateUserProcess`) and
/// immediately terminates it WITHOUT ever calling `ResumeThread`, so zero instructions of
/// untrusted code execute during prewarm.
pub fn prewarm_binary(exe_path: &Path, work_dir: &Path) {
    let spec = RunSpec {
        argv: vec![exe_path.into()],
        env: vec![],
        work_dir: work_dir.to_path_buf(),
        ro_binds: vec![],
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Null,
        extra_fds: vec![],
        limits: uj_core::run::Limits::new(500, 256 * 1024 * 1024),
        cpu_affinity: None,
        seccomp_profile: Default::default(),
        trusted: true,
    };

    let (h_stdin, c1) = match prepare_stdin(&spec.stdin) {
        Ok(v) => v,
        Err(_) => return,
    };
    let (h_stdout, _, c2) = match prepare_stdout(&spec.stdout) {
        Ok(v) => v,
        Err(_) => return,
    };
    let (h_stderr, _, c3) = match prepare_stderr(&spec.stderr) {
        Ok(v) => v,
        Err(_) => return,
    };

    let pi = unsafe { spawn_suspended_process(&spec, h_stdin, h_stdout, h_stderr, None) };

    drop(c1);
    drop(c2);
    drop(c3);

    if let Ok(pi) = pi {
        unsafe {
            let _ = TerminateProcess(pi.hProcess, 0);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
    }
}

/// Detect primary logical CPU IDs for physical P-cores on Windows.
/// Uses `GetLogicalProcessorInformationEx` with `RelationProcessorCore`,
/// filters to maximum `EfficiencyClass` (P-cores only on hybrid Intel),
/// and selects the primary CPU for each core using `mask.trailing_zeros()`
/// (skipping hyperthreading siblings).
pub fn detect_p_core_primary_cpus() -> Vec<usize> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    let mut len: u32 = 0;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
    }
    if len == 0 {
        return (0..num_cpus::get()).collect();
    }

    let mut buffer = vec![0u8; len as usize];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
    };
    if ok.is_err() {
        return (0..num_cpus::get()).collect();
    }

    struct CoreInfo {
        efficiency_class: u8,
        primary_cpu: usize,
    }

    let mut cores: Vec<CoreInfo> = Vec::new();
    let mut offset = 0;
    while offset + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= len as usize {
        let ptr = unsafe {
            buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX
        };
        let record = unsafe { &*ptr };
        if record.Relationship == RelationProcessorCore {
            let core = unsafe { record.Anonymous.Processor };
            let mask = core.GroupMask[0].Mask;
            let efficiency_class = core.EfficiencyClass;
            let primary_cpu = mask.trailing_zeros() as usize;
            cores.push(CoreInfo {
                efficiency_class,
                primary_cpu,
            });
        }
        offset += record.Size as usize;
    }

    if cores.is_empty() {
        return (0..num_cpus::get()).collect();
    }

    let max_class = cores.iter().map(|c| c.efficiency_class).max().unwrap_or(0);
    let mut p_cores: Vec<usize> = cores
        .into_iter()
        .filter(|c| c.efficiency_class == max_class)
        .map(|c| c.primary_cpu)
        .collect();
    p_cores.sort_unstable();
    p_cores.dedup();
    if p_cores.is_empty() {
        (0..num_cpus::get()).collect()
    } else {
        p_cores
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PipeConnection, Wiring};
    use uj_core::run::Limits;

    #[test]
    fn test_build_windows_cmd_line_msvc_escaping() {
        use std::ffi::OsString;
        // 1. Simple args without spaces or quotes
        assert_eq!(
            build_windows_cmd_line(&[OsString::from("prog.exe"), OsString::from("arg1")]),
            "prog.exe arg1"
        );
        // 2. Trailing backslash with space (must double trailing backslash before closing quote)
        assert_eq!(
            build_windows_cmd_line(&[
                OsString::from("prog.exe"),
                OsString::from(r"C:\dir with space\"),
            ]),
            r#"prog.exe "C:\dir with space\\""#
        );
        // 3. Embedded quotes and backslashes preceding quotes
        assert_eq!(
            build_windows_cmd_line(&[
                OsString::from("prog.exe"),
                OsString::from(r#"a\"b"#),
                OsString::from(r#"he said "hi""#),
            ]),
            r#"prog.exe "a\\\"b" "he said \"hi\"""#
        );
        // 4. Empty argument and newline/tab
        assert_eq!(
            build_windows_cmd_line(&[OsString::from(""), OsString::from("line1\nline2")]),
            "\"\" \"line1\nline2\""
        );
    }

    #[test]
    fn test_execute_group_partial_spawn_failure_no_handle_leak() {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

        let temp_dir =
            std::env::temp_dir().join(format!("uj_test_grp_leak_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let spec0 = RunSpec {
            argv: vec!["cmd.exe".into(), "/c".into(), "exit".into(), "0".into()],
            env: vec![],
            work_dir: temp_dir.clone(),
            ro_binds: vec![],
            stdin: Stdio::Pipe,
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits: Limits::new(1000, 128 * 1024 * 1024),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: true,
        };
        let spec1 = RunSpec {
            argv: vec![temp_dir.join("non_existent_binary_12345.exe").into()],
            env: vec![],
            work_dir: temp_dir.clone(),
            ro_binds: vec![],
            stdin: Stdio::Pipe,
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits: Limits::new(1000, 128 * 1024 * 1024),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: true,
        };

        let wiring = Wiring {
            connections: vec![
                PipeConnection {
                    from_proc: 0,
                    to_proc: 1,
                },
                PipeConnection {
                    from_proc: 1,
                    to_proc: 0,
                },
            ],
            star_manager_proc: None,
        };
        let cancel = CancelToken::new();

        // Warm-up run
        let _ = execute_group_in_jobs(
            &[spec0.clone(), spec1.clone()],
            &wiring,
            None,
            &cancel,
            Tier::Degraded,
        );

        let mut before_handles = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(GetCurrentProcess(), &mut before_handles);
        }

        for _ in 0..30 {
            let res = execute_group_in_jobs(
                &[spec0.clone(), spec1.clone()],
                &wiring,
                None,
                &cancel,
                Tier::Degraded,
            );
            assert!(res.is_err());
        }

        let mut after_handles = 0u32;
        unsafe {
            let _ = GetProcessHandleCount(GetCurrentProcess(), &mut after_handles);
        }
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Allow tiny noise from background test threads, but 30 iterations leaking 6+ handles each would be +180 handles
        assert!(
            after_handles <= before_handles + 15,
            "Handle leak detected! before={before_handles}, after={after_handles}"
        );
    }

    #[test]
    fn test_execute_group_duplicate_wiring_rejected() {
        let temp_dir = std::env::temp_dir();
        let spec = RunSpec {
            argv: vec!["cmd.exe".into()],
            env: vec![],
            work_dir: temp_dir,
            ro_binds: vec![],
            stdin: Stdio::Null,
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            extra_fds: vec![],
            limits: Limits::new(1000, 128 * 1024 * 1024),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: true,
        };
        let dup_wiring = Wiring {
            connections: vec![
                PipeConnection {
                    from_proc: 0,
                    to_proc: 1,
                },
                PipeConnection {
                    from_proc: 0,
                    to_proc: 1,
                },
            ],
            star_manager_proc: None,
        };
        let cancel = CancelToken::new();
        let res = execute_group_in_jobs(
            &[spec.clone(), spec],
            &dup_wiring,
            None,
            &cancel,
            Tier::Degraded,
        );
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("Duplicate stdio wiring")
        );
    }
}

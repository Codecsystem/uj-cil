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
use crate::{CancelToken, SandboxError, Wiring};

// Thread attribute keys:
pub const ATTR_HANDLE_LIST: usize = 0x00020002;
pub const ATTR_SECURITY_CAPABILITIES: usize = 0x00020009;
pub const ATTR_CHILD_PROCESS_POLICY: usize = 0x0002000E;
pub const PROCESS_CREATION_CHILD_PROCESS_RESTRICTED: u32 = 0x01;

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

    let exe_path =
        which::which(&spec.argv[0]).unwrap_or_else(|_| std::path::PathBuf::from(&spec.argv[0]));
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

    unsafe {
        windows::Win32::System::Diagnostics::Debug::SetErrorMode(
            windows::Win32::System::Diagnostics::Debug::SEM_FAILCRITICALERRORS
                | windows::Win32::System::Diagnostics::Debug::SEM_NOGPFAULTERRORBOX
                | windows::Win32::System::Diagnostics::Debug::SEM_NOOPENFILEERRORBOX,
        );
    }

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
    let (h_stdout, stdout_read_handle, stdout_cleaner) = prepare_stdout(&spec.stdout)?;
    let (h_stderr, stderr_read_handle, stderr_cleaner) = prepare_stderr(&spec.stderr)?;

    let pi = unsafe { spawn_suspended_process(spec, h_stdin, h_stdout, h_stderr, app_container)? };

    // Close the write end of pipes in the parent so reader threads receive EOF when child exits!
    drop(stdin_cleaner);
    drop(stdout_cleaner);
    drop(stderr_cleaner);

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

        // 5. Check if process terminated
        let wait_res = unsafe { WaitForSingleObject(pi.hProcess, 5) };
        if wait_res == WAIT_OBJECT_0 {
            break;
        }

        // 6. Memory check
        if job.check_memory_limit_violation() {
            killed_by = Some(KillReason::MemoryLimit);
            let _ = job.terminate(3);
            break;
        }

        if let Ok(peak_mem) = job.query_peak_memory_bytes()
            && peak_mem > spec.limits.memory_bytes
        {
            killed_by = Some(KillReason::MemoryLimit);
            let _ = job.terminate(3);
            break;
        }
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
        || memory_peak_bytes > spec.limits.memory_bytes;

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

    // 2. Prepare I/O for each process
    let mut stdin_handles = Vec::with_capacity(n);
    let mut stdout_handles = Vec::with_capacity(n);
    let mut stderr_handles = Vec::with_capacity(n);
    let mut stdout_read_handles = Vec::with_capacity(n);
    let mut stderr_read_handles = Vec::with_capacity(n);
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
        stdout_read_handles.push(r_out);
        if let Some(c) = out_cleaner {
            extra_cleaners.push(c);
        }

        let (h_err, r_err, err_cleaner) = prepare_stderr(&specs[i].stderr)?;
        stderr_handles.push(h_err);
        stderr_read_handles.push(r_err);
        if let Some(c) = err_cleaner {
            extra_cleaners.push(c);
        }
    }

    // 3. Create Job Objects and Spawn suspended processes
    let mut jobs = Vec::with_capacity(n);
    let mut proc_infos = Vec::with_capacity(n);

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
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
            }
            return Err(SandboxError::Internal(format!(
                "AssignProcessToJobObject for proc {i} failed: {e}"
            )));
        }

        jobs.push(job);
        proc_infos.push(pi);
    }

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
        let s_thread = if let Some(r_handle) = stdout_read_handles[i] {
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
        let e_thread = if let Some(r_handle) = stderr_read_handles[i] {
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
    let poll_interval = Duration::from_millis(5);

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

        // 3. Check each process
        for i in 0..n {
            if finished[i] {
                continue;
            }

            // OLE check
            if ole_flags[i].load(Ordering::SeqCst) {
                killed_by[i] = Some(KillReason::OutputLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                exit_order[i] = Some(order_counter);
                order_counter += 1;
                continue;
            }

            // CPU check
            if let Ok(cpu_us) = jobs[i].query_cpu_time_us()
                && (cpu_us / 1000) > specs[i].limits.cpu_time_ms
            {
                killed_by[i] = Some(KillReason::CpuLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                exit_order[i] = Some(order_counter);
                order_counter += 1;
                continue;
            }

            // Process exit check
            let wait_res = unsafe { WaitForSingleObject(proc_infos[i].hProcess, 0) };
            if wait_res == WAIT_OBJECT_0 {
                finished[i] = true;
                exit_order[i] = Some(order_counter);
                order_counter += 1;

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
                final_wall_us[i] = start_time.elapsed().as_micros() as u64;
                continue;
            }

            // Memory check
            if jobs[i].check_memory_limit_violation() {
                killed_by[i] = Some(KillReason::MemoryLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                exit_order[i] = Some(order_counter);
                order_counter += 1;
                continue;
            }

            if let Ok(mem) = jobs[i].query_peak_memory_bytes()
                && mem > specs[i].limits.memory_bytes
            {
                killed_by[i] = Some(KillReason::MemoryLimit);
                let _ = jobs[i].terminate(3);
                finished[i] = true;
                exit_order[i] = Some(order_counter);
                order_counter += 1;
                continue;
            }
        }

        thread::sleep(poll_interval);
    }

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
            final_wall_us[i] = start_time.elapsed().as_micros() as u64;
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
            || final_peak_mem[i] > specs[i].limits.memory_bytes;

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
            sandbox_tier: if i == 0 { tier } else { Tier::Degraded },
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

/// Pre-warm newly prepared binary to trigger Windows Defender scan ahead of timed execution.
/// Runs the binary briefly inside AppContainer with CREATE_NO_WINDOW and waits up to 100ms or until it finishes,
/// then cleanly terminates it.
pub fn prewarm_binary(exe_path: &Path, work_dir: &Path) {
    let app_container = AppContainerProfile::get_shared().ok();
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
        trusted: false,
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

    let pi = unsafe {
        spawn_suspended_process(&spec, h_stdin, h_stdout, h_stderr, app_container.as_deref())
    };

    drop(c1);
    drop(c2);
    drop(c3);

    let job = JobObject::create(None).ok();
    if let Some(ref j) = job {
        let _ = j.set_limits(spec.limits.memory_bytes, 1, spec.cpu_affinity.as_deref());
    }

    if let Ok(pi) = pi {
        if let Some(ref j) = job {
            let _ = j.assign_process(pi.hProcess);
        }
        unsafe {
            let _ = ResumeThread(pi.hThread);
            let _ = WaitForSingleObject(pi.hProcess, 50);
            if let Some(ref j) = job {
                let _ = j.terminate(0);
            } else {
                let _ = windows::Win32::System::Threading::TerminateProcess(pi.hProcess, 0);
            }
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

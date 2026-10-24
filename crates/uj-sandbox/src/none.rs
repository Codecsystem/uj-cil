//! NoneSandbox: Minimal isolation fallback backend for testing and development.
//! Uses standard OS process execution with thread-based monitoring and I/O pump.

use std::fs::File;
use std::io::Read;
#[allow(unused_imports)]
use std::process::{Child, Command, Stdio as StdProcessStdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};

#[cfg(not(unix))]
use crate::ntstatus::is_ntstatus_exception;
use crate::{CancelToken, CleanupReport, Sandbox, SandboxError, Wiring};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

pub struct NoneSandbox;

impl NoneSandbox {
    pub fn new() -> Self {
        Self
    }

    fn run_impl(
        &self,
        spec: &RunSpec,
        cancel: &CancelToken,
        #[cfg(unix)] custom_stdin: Option<OwnedFd>,
        #[cfg(unix)] custom_stdout: Option<OwnedFd>,
        #[cfg(unix)] extra_owned_fds: Vec<OwnedFd>,
    ) -> Result<RunResult, SandboxError> {
        if spec.argv.is_empty() {
            return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
        }

        let exe =
            which::which(&spec.argv[0]).unwrap_or_else(|_| std::path::PathBuf::from(&spec.argv[0]));
        let mut cmd = Command::new(exe);
        if spec.argv.len() > 1 {
            cmd.args(&spec.argv[1..]);
        }

        cmd.current_dir(&spec.work_dir);

        // Minimal environment
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        // Stdio setup
        #[cfg(unix)]
        if let Some(fd) = custom_stdin {
            cmd.stdin(StdProcessStdio::from(fd));
        } else {
            match &spec.stdin {
                Stdio::Null => {
                    cmd.stdin(StdProcessStdio::null());
                }
                Stdio::File(path) => {
                    let file = File::open(path)?;
                    cmd.stdin(StdProcessStdio::from(file));
                }
                Stdio::Pipe => {
                    cmd.stdin(StdProcessStdio::piped());
                }
                Stdio::Inherit => {
                    cmd.stdin(StdProcessStdio::inherit());
                }
            }
        }
        #[cfg(not(unix))]
        match &spec.stdin {
            Stdio::Null => {
                cmd.stdin(StdProcessStdio::null());
            }
            Stdio::File(path) => {
                let file = File::open(path)?;
                cmd.stdin(StdProcessStdio::from(file));
            }
            Stdio::Pipe => {
                cmd.stdin(StdProcessStdio::piped());
            }
            Stdio::Inherit => {
                cmd.stdin(StdProcessStdio::inherit());
            }
        }

        #[cfg(unix)]
        let capture_stdout = custom_stdout.is_none() && matches!(&spec.stdout, Stdio::Pipe);
        #[cfg(not(unix))]
        let capture_stdout = matches!(&spec.stdout, Stdio::Pipe);

        #[cfg(unix)]
        if let Some(fd) = custom_stdout {
            cmd.stdout(StdProcessStdio::from(fd));
        } else {
            match &spec.stdout {
                Stdio::Null => {
                    cmd.stdout(StdProcessStdio::null());
                }
                Stdio::File(path) => {
                    let file = File::create(path)?;
                    cmd.stdout(StdProcessStdio::from(file));
                }
                Stdio::Pipe => {
                    cmd.stdout(StdProcessStdio::piped());
                }
                Stdio::Inherit => {
                    cmd.stdout(StdProcessStdio::inherit());
                }
            }
        }
        #[cfg(not(unix))]
        match &spec.stdout {
            Stdio::Null => {
                cmd.stdout(StdProcessStdio::null());
            }
            Stdio::File(path) => {
                let file = File::create(path)?;
                cmd.stdout(StdProcessStdio::from(file));
            }
            Stdio::Pipe => {
                cmd.stdout(StdProcessStdio::piped());
            }
            Stdio::Inherit => {
                cmd.stdout(StdProcessStdio::inherit());
            }
        }

        // Always pipe stderr to capture diagnostics
        cmd.stderr(StdProcessStdio::piped());

        #[cfg(unix)]
        if !spec.extra_fds.is_empty() {
            let extra_raw_fds: Vec<i32> = spec.extra_fds.iter().map(|f| f.0 as i32).collect();
            unsafe {
                cmd.pre_exec(move || {
                    let min_safe = 3 + extra_raw_fds.len() as i32 + 10;
                    let mut temp_fds = Vec::with_capacity(extra_raw_fds.len());
                    for &src_fd in &extra_raw_fds {
                        let dup_fd = libc::fcntl(src_fd, libc::F_DUPFD_CLOEXEC, min_safe);
                        if dup_fd < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        temp_fds.push(dup_fd);
                    }
                    for (idx, &tfd) in temp_fds.iter().enumerate() {
                        let target_fd = 3 + idx as i32;
                        if libc::dup2(tfd, target_fd) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        libc::close(tfd);
                    }
                    Ok(())
                });
            }
        }

        let start_time = Instant::now();
        #[allow(unused_mut)]
        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::Internal(format!("Failed to spawn child: {e}")))?;

        #[cfg(unix)]
        drop(extra_owned_fds);

        // Output capture channels and threads
        let stdout_limit = spec.limits.output_bytes as usize;
        let stderr_limit = spec.limits.stderr_bytes as usize;
        let ole_flag = Arc::new(AtomicBool::new(false));

        let stdout_handle = if capture_stdout {
            let mut stdout_pipe = child.stdout.take();
            let ole_flag_clone = Arc::clone(&ole_flag);
            Some(thread::spawn(move || {
                let mut buf = Vec::with_capacity(stdout_limit.min(65536));
                if let Some(mut stream) = stdout_pipe.take() {
                    let mut chunk = vec![0u8; 65536];
                    while let Ok(n) = stream.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        if buf.len() + n > stdout_limit {
                            ole_flag_clone.store(true, Ordering::SeqCst);
                            let keep = stdout_limit.saturating_sub(buf.len());
                            buf.extend_from_slice(&chunk[..keep]);
                            break;
                        } else {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
                buf
            }))
        } else {
            None
        };

        let mut stderr_pipe = child.stderr.take();
        let stderr_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut stream) = stderr_pipe.take() {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if buf.len() < stderr_limit {
                        let take = (stderr_limit - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
            }
            buf
        });

        // Monitoring loop
        let mut killed_by: Option<KillReason> = None;
        let poll_interval = Duration::from_millis(5);

        #[cfg(unix)]
        let (status, cpu_time_us, memory_peak_bytes) = {
            let pid = child.id() as i32;
            let (raw_status, cpu_us, mem_bytes) = loop {
                if cancel.is_cancelled() {
                    killed_by = Some(KillReason::UserCancel);
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    break wait4_child_unix(pid, false)?.unwrap_or((libc::SIGKILL, 0, 0));
                }

                if ole_flag.load(Ordering::SeqCst) {
                    killed_by = Some(KillReason::OutputLimit);
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    break wait4_child_unix(pid, false)?.unwrap_or((libc::SIGKILL, 0, 0));
                }

                let elapsed_wall_ms = start_time.elapsed().as_millis() as u64;
                if elapsed_wall_ms > spec.limits.wall_time_ms {
                    killed_by = Some(KillReason::WallLimit);
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    break wait4_child_unix(pid, false)?.unwrap_or((libc::SIGKILL, 0, 0));
                }

                if let Some(res) = wait4_child_unix(pid, true)? {
                    break res;
                }

                thread::sleep(poll_interval);
            };

            let exit_status = if libc::WIFSIGNALED(raw_status) {
                ExitStatus::Signaled(libc::WTERMSIG(raw_status))
            } else if libc::WIFEXITED(raw_status) {
                ExitStatus::Exited(libc::WEXITSTATUS(raw_status))
            } else {
                ExitStatus::Exited(-1)
            };

            (exit_status, cpu_us, mem_bytes)
        };

        #[cfg(not(unix))]
        let (status, cpu_time_us, memory_peak_bytes) = {
            let final_status = loop {
                // Check cancellation
                if cancel.is_cancelled() {
                    killed_by = Some(KillReason::UserCancel);
                    let _ = child.kill();
                    break child.wait()?;
                }

                // Check output limit
                if ole_flag.load(Ordering::SeqCst) {
                    killed_by = Some(KillReason::OutputLimit);
                    let _ = child.kill();
                    break child.wait()?;
                }

                // Check wall clock time
                let elapsed_wall_ms = start_time.elapsed().as_millis() as u64;
                if elapsed_wall_ms > spec.limits.wall_time_ms {
                    killed_by = Some(KillReason::WallLimit);
                    let _ = child.kill();
                    break child.wait()?;
                }

                // Check CPU time if supported on platform
                #[cfg(windows)]
                {
                    if let Some(cpu_us) = get_child_cpu_time_us(&child)
                        && (cpu_us / 1000) > spec.limits.cpu_time_ms
                    {
                        killed_by = Some(KillReason::CpuLimit);
                        let _ = child.kill();
                        break child.wait()?;
                    }
                }

                if let Some(status) = child.try_wait()? {
                    break status;
                }

                thread::sleep(poll_interval);
            };

            let wall_us_now = start_time.elapsed().as_micros() as u64;
            #[cfg(windows)]
            let (cpu_us, mem_bytes) =
                get_child_cpu_and_mem_windows(&child).unwrap_or((wall_us_now, 0));
            #[cfg(not(windows))]
            let (cpu_us, mem_bytes) = (wall_us_now, 0);

            let exit_code = final_status.code().unwrap_or(-1);
            let raw_code = exit_code as u32;
            let exit_status = if is_ntstatus_exception(raw_code) {
                ExitStatus::NtStatus(raw_code)
            } else {
                ExitStatus::Exited(exit_code)
            };

            (exit_status, cpu_us, mem_bytes)
        };

        let wall_time_us = start_time.elapsed().as_micros() as u64;

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

        let stderr_tail = stderr_handle.join().unwrap_or_default();

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
            sandbox_tier: Tier::None,
            exit_order: None,
        })
    }

    #[cfg(unix)]
    fn execute_group_unix(
        &self,
        specs: &[RunSpec],
        wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError> {
        let n = specs.len();
        let mut specs = specs.to_vec();
        let mut wired_stdins: Vec<Option<OwnedFd>> = (0..n).map(|_| None).collect();
        let mut wired_stdouts: Vec<Option<OwnedFd>> = (0..n).map(|_| None).collect();
        let mut extra_owned: Vec<Vec<OwnedFd>> = (0..n).map(|_| Vec::new()).collect();

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
            let mut fds = [0i32; 2];
            let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            if res != 0 {
                return Err(SandboxError::Io(std::io::Error::last_os_error()));
            }
            let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            wired_stdouts[conn.from_proc] = Some(write_fd);
            wired_stdins[conn.to_proc] = Some(read_fd);
        }

        if let Some(mgr_idx) = wiring.star_manager_proc {
            if mgr_idx >= n {
                return Err(SandboxError::Internal(format!(
                    "Invalid star_manager_proc {} (total procs: {})",
                    mgr_idx, n
                )));
            }
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
                let mut fds1 = [0i32; 2];
                if unsafe { libc::pipe2(fds1.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                    return Err(SandboxError::Io(std::io::Error::last_os_error()));
                }
                let p_to_m_read = unsafe { OwnedFd::from_raw_fd(fds1[0]) };
                let p_to_m_write = unsafe { OwnedFd::from_raw_fd(fds1[1]) };

                let mut fds2 = [0i32; 2];
                if unsafe { libc::pipe2(fds2.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                    return Err(SandboxError::Io(std::io::Error::last_os_error()));
                }
                let m_to_p_read = unsafe { OwnedFd::from_raw_fd(fds2[0]) };
                let m_to_p_write = unsafe { OwnedFd::from_raw_fd(fds2[1]) };

                wired_stdouts[p] = Some(p_to_m_write);
                wired_stdins[p] = Some(m_to_p_read);

                let r_target = 3 + specs[mgr_idx].extra_fds.len();
                let w_target = r_target + 1;
                specs[mgr_idx]
                    .extra_fds
                    .push(uj_core::run::RawFdOrHandle(p_to_m_read.as_raw_fd() as usize));
                specs[mgr_idx].extra_fds.push(uj_core::run::RawFdOrHandle(
                    m_to_p_write.as_raw_fd() as usize,
                ));
                extra_owned[mgr_idx].push(p_to_m_read);
                extra_owned[mgr_idx].push(m_to_p_write);

                specs[mgr_idx]
                    .argv
                    .push(std::ffi::OsString::from(r_target.to_string()));
                specs[mgr_idx]
                    .argv
                    .push(std::ffi::OsString::from(w_target.to_string()));
                star_fds_env.push(format!("{r_target},{w_target}"));
            }
            specs[mgr_idx].env.push((
                std::ffi::OsString::from("UJ_STAR_FDS"),
                std::ffi::OsString::from(star_fds_env.join(";")),
            ));
        }

        // Run specs concurrently in worker threads
        thread::scope(|s| {
            let mut handles = Vec::new();
            for (i, spec) in specs.iter().enumerate() {
                let in_fd = wired_stdins[i].take();
                let out_fd = wired_stdouts[i].take();
                let ex_fds = std::mem::take(&mut extra_owned[i]);
                handles.push(s.spawn(move || self.run_impl(spec, cancel, in_fd, out_fd, ex_fds)));
            }

            let mut res = Vec::new();
            for h in handles {
                match h.join() {
                    Ok(Ok(run_res)) => res.push(run_res),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        return Err(SandboxError::Internal(
                            "Thread panicked in execute_group_unix".into(),
                        ));
                    }
                }
            }
            Ok(res)
        })
    }
}

impl Default for NoneSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for NoneSandbox {
    fn tier(&self) -> Tier {
        Tier::None
    }

    fn run(&self, spec: &RunSpec, cancel: &CancelToken) -> Result<RunResult, SandboxError> {
        #[cfg(unix)]
        {
            self.run_impl(spec, cancel, None, None, Vec::new())
        }
        #[cfg(not(unix))]
        {
            self.run_impl(spec, cancel)
        }
    }

    fn run_group(
        &self,
        specs: &[RunSpec],
        wiring: &Wiring,
        cancel: &CancelToken,
    ) -> Result<Vec<RunResult>, SandboxError> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(windows)]
        {
            crate::windows::process::execute_group_in_jobs(specs, wiring, None, cancel, Tier::None)
        }
        #[cfg(unix)]
        {
            self.execute_group_unix(specs, wiring, cancel)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let mut results = Vec::with_capacity(specs.len());
            for spec in specs {
                results.push(self.run(spec, cancel)?);
            }
            Ok(results)
        }
    }

    fn cleanup_stale(&self) -> Result<CleanupReport, SandboxError> {
        Ok(CleanupReport::default())
    }
}

#[cfg(unix)]
fn wait4_child_unix(pid: i32, nohang: bool) -> std::io::Result<Option<(i32, u64, u64)>> {
    let options = if nohang { libc::WNOHANG } else { 0 };
    let mut status: i32 = 0;
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let ret = unsafe { libc::wait4(pid, &mut status, options, &mut rusage) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Ok(None);
        }
        let utime_us = (rusage.ru_utime.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(rusage.ru_utime.tv_usec as u64);
        let stime_us = (rusage.ru_stime.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(rusage.ru_stime.tv_usec as u64);
        let cpu_us = utime_us.saturating_add(stime_us);
        let mem_bytes = (rusage.ru_maxrss as u64).saturating_mul(1024);
        return Ok(Some((status, cpu_us, mem_bytes)));
    }
}

#[cfg(windows)]
fn get_child_cpu_time_us(child: &Child) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::Threading::GetProcessTimes;

    let handle = HANDLE(child.as_raw_handle());
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    unsafe {
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok() {
            let k = ((kernel.dwHighDateTime as u64) << 32) | (kernel.dwLowDateTime as u64);
            let u = ((user.dwHighDateTime as u64) << 32) | (user.dwLowDateTime as u64);
            // 100-nanosecond intervals to microseconds
            Some((k + u) / 10)
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn get_child_cpu_and_mem_windows(child: &Child) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetProcessTimes;

    let handle = HANDLE(child.as_raw_handle());
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    let cpu_us = unsafe {
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok() {
            let k = ((kernel.dwHighDateTime as u64) << 32) | (kernel.dwLowDateTime as u64);
            let u = ((user.dwHighDateTime as u64) << 32) | (user.dwLowDateTime as u64);
            (k + u) / 10
        } else {
            0
        }
    };

    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    let mem_bytes = unsafe {
        if GetProcessMemoryInfo(
            handle,
            &mut pmc,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .is_ok()
        {
            pmc.PeakPagefileUsage as u64
        } else {
            0
        }
    };

    Some((cpu_us, mem_bytes))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use uj_core::run::Limits;

    #[test]
    fn test_none_sandbox_unix_signal_and_rusage() {
        let sb = NoneSandbox::new();
        let cancel = CancelToken::new();
        let spec = RunSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "kill -SEGV $$".into()],
            env: vec![],
            work_dir: std::env::temp_dir(),
            ro_binds: vec![],
            stdin: Stdio::Null,
            stdout: Stdio::Pipe,
            stderr: Stdio::Pipe,
            extra_fds: vec![],
            limits: Limits::new(2000, 128 * 1024 * 1024),
            cpu_affinity: None,
            seccomp_profile: Default::default(),
            trusted: true,
        };
        let res = sb.run(&spec, &cancel).unwrap();
        assert_eq!(res.status, ExitStatus::Signaled(libc::SIGSEGV));
        assert!(res.memory_peak_bytes > 0);
    }
}

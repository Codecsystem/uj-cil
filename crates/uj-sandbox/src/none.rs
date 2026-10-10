//! NoneSandbox: Minimal isolation fallback backend for testing and development.
//! Uses standard OS process execution with thread-based monitoring and I/O pump.

use std::fs::File;
use std::io::Read;
use std::process::{Child, Command, Stdio as StdProcessStdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};

use crate::ntstatus::is_ntstatus_exception;
use crate::{CancelToken, CleanupReport, Sandbox, SandboxError, Wiring};

pub struct NoneSandbox;

impl NoneSandbox {
    pub fn new() -> Self {
        Self
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
        if spec.argv.is_empty() {
            return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
        }

        let mut cmd = Command::new(&spec.argv[0]);
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

        let capture_stdout = matches!(&spec.stdout, Stdio::Pipe);
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

        let start_time = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::Internal(format!("Failed to spawn child: {e}")))?;

        // Output capture channels and threads
        let stdout_limit = spec.limits.output_bytes as usize;
        let stderr_limit = spec.limits.stderr_bytes as usize;
        let ole_flag = Arc::new(AtomicBool::new(false));

        let stdout_handle = if capture_stdout {
            let mut stdout_pipe = child.stdout.take();
            let ole_flag_clone = Arc::clone(&ole_flag);
            Some(thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut stream) = stdout_pipe.take() {
                    let mut chunk = [0u8; 8192];
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
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > stderr_limit * 2 {
                        let excess = buf.len() - stderr_limit;
                        buf.drain(0..excess);
                    }
                }
            }
            if buf.len() > stderr_limit {
                let excess = buf.len() - stderr_limit;
                buf.drain(0..excess);
            }
            buf
        });

        // Monitoring loop
        let mut killed_by: Option<KillReason> = None;
        let poll_interval = Duration::from_millis(5);

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

        let wall_time_us = start_time.elapsed().as_micros() as u64;

        // Query final CPU and memory usage
        #[cfg(windows)]
        let (cpu_time_us, memory_peak_bytes) =
            get_child_cpu_and_mem_windows(&child).unwrap_or((wall_time_us, 0));
        #[cfg(not(windows))]
        let (cpu_time_us, memory_peak_bytes) = (wall_time_us, 0);

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

        let exit_code = final_status.code().unwrap_or(-1);
        let raw_code = exit_code as u32;

        let status = if is_ntstatus_exception(raw_code) {
            ExitStatus::NtStatus(raw_code)
        } else {
            ExitStatus::Exited(exit_code)
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
            sandbox_tier: Tier::None,
            exit_order: None,
        })
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
        #[cfg(not(windows))]
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

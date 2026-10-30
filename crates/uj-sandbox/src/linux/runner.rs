//! Process runner and namespace isolation for Linux sandbox.

use std::fs::File;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio as StdProcessStdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use crate::linux::cgroup::CgroupV2;
use crate::{CancelToken, SandboxError, Wiring};
use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};

/// Packet sent from container init (PID 1) to supervisor process over anonymous pipe
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SandboxStatusPacket {
    is_signaled: u8,
    pad: [u8; 3],
    code_or_sig: i32,
    ru_utime_us: u64,
    ru_stime_us: u64,
    ru_maxrss_kb: u64,
}

#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;
// SECBIT_NOROOT (1) | SECBIT_NOROOT_LOCKED (2) | SECBIT_NO_SETUID_FIXUP (4)
// | SECBIT_NO_SETUID_FIXUP_LOCKED (8) | SECBIT_KEEP_CAPS_LOCKED (32) = 0x2f
const SECUREBITS_LOCKED_NO_ROOT: libc::c_ulong = 0x2f;

#[derive(Clone)]
struct SendSyncPtrs {
    _storage: Vec<std::ffi::CString>,
    ptrs: Vec<*const libc::c_char>,
}
unsafe impl Send for SendSyncPtrs {}
unsafe impl Sync for SendSyncPtrs {}

impl SendSyncPtrs {
    pub fn new(cstrings: Vec<std::ffi::CString>) -> Self {
        let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|s| s.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Self {
            _storage: cstrings,
            ptrs,
        }
    }

    pub fn empty() -> Self {
        Self {
            _storage: Vec::new(),
            ptrs: vec![std::ptr::null()],
        }
    }

    pub fn as_ptr(&self) -> *const *const libc::c_char {
        self.ptrs.as_ptr()
    }
}

/// Helper to open a pidfd on Linux (syscall 434).
fn pidfd_open(pid: i32) -> Result<i32, std::io::Error> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

/// Waits for a child PID using `wait4` to capture `rusage` alongside `ExitStatus`.
fn wait4_pid(
    pid: i32,
    options: i32,
) -> std::io::Result<Option<(std::process::ExitStatus, libc::rusage)>> {
    unsafe {
        let mut status: libc::c_int = 0;
        let mut rusage: libc::rusage = std::mem::zeroed();
        loop {
            let ret = libc::wait4(pid, &mut status, options, &mut rusage);
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            if ret == 0 {
                return Ok(None);
            }
            return Ok(Some((std::process::ExitStatus::from_raw(status), rusage)));
        }
    }
}

/// Executes a single process in Linux sandbox (cgroup v2 + namespaces).
pub fn execute_in_sandbox(
    spec: &RunSpec,
    cgroup_base: Option<&Path>,
    cancel: &CancelToken,
    tier: Tier,
) -> Result<RunResult, SandboxError> {
    execute_in_sandbox_with_stdio(spec, cgroup_base, cancel, tier, None, None, Vec::new())
}

/// Executes a single process in Linux sandbox with optional owned stdin/stdout/extra fds.
pub fn execute_in_sandbox_with_stdio(
    spec: &RunSpec,
    cgroup_base: Option<&Path>,
    cancel: &CancelToken,
    tier: Tier,
    custom_stdin: Option<OwnedFd>,
    custom_stdout: Option<OwnedFd>,
    extra_owned_fds: Vec<OwnedFd>,
) -> Result<RunResult, SandboxError> {
    if spec.argv.is_empty() {
        return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
    }

    let is_trusted = spec.trusted;
    let used_namespaces = !is_trusted && (tier == Tier::Full || tier == Tier::Degraded);

    // 1. Create dedicated cgroup v2 if cgroup_base is provided
    let cgroup = if let Some(base) = cgroup_base {
        let cg = CgroupV2::create_for_run(base, "uj")?;
        cg.set_limits(&spec.limits, spec.cpu_affinity.as_deref())?;
        Some(cg)
    } else {
        None
    };

    // 2. Prepare Command
    let exe_path = which::which(&spec.argv[0]).unwrap_or_else(|_| PathBuf::from(&spec.argv[0]));
    let mut cmd = Command::new(&exe_path);
    if spec.argv.len() > 1 {
        cmd.args(&spec.argv[1..]);
    }
    cmd.current_dir(&spec.work_dir);

    // Environment
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Stdio
    // For untrusted namespace runs with Stdio::File(path), we do NOT pass a host rw-mount fd
    // directly into the container (which would allow reopening /proc/self/fd/0 with O_RDWR).
    // Instead, we validate readability here, bind-mount the input file read-only inside the jail,
    // open it O_RDONLY from the read-only bind mount after pivot_root, and detach/unlink the mount point.
    let mut jail_stdin_host_path: Option<PathBuf> = None;
    if let Some(fd) = custom_stdin {
        cmd.stdin(StdProcessStdio::from(fd));
    } else {
        match &spec.stdin {
            Stdio::Null => {
                cmd.stdin(StdProcessStdio::null());
            }
            Stdio::File(path) => {
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    spec.work_dir.join(path)
                };
                if used_namespaces {
                    // Verify input file exists and is readable before spawning
                    let _ = File::open(&resolved)?;
                    jail_stdin_host_path = Some(resolved);
                    cmd.stdin(StdProcessStdio::null());
                } else {
                    let file = File::open(&resolved)?;
                    cmd.stdin(StdProcessStdio::from(file));
                }
            }
            Stdio::Pipe => {
                cmd.stdin(StdProcessStdio::piped());
            }
            Stdio::Inherit => {
                cmd.stdin(StdProcessStdio::inherit());
            }
        }
    }

    let capture_stdout = custom_stdout.is_none() && matches!(&spec.stdout, Stdio::Pipe);
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

    cmd.stderr(StdProcessStdio::piped());

    // 3. Configure Namespace & Isolation via pre_exec
    let work_dir = spec.work_dir.clone();
    let stack_limit = spec.limits.stack_bytes.unwrap_or_else(|| {
        spec.limits
            .memory_bytes
            .saturating_sub(4 * 1024 * 1024)
            .max(8 * 1024 * 1024)
    });
    let extra_raw_fds: Vec<i32> = spec.extra_fds.iter().map(|f| f.0 as i32).collect();

    let host_exe = if exe_path.is_absolute() {
        exe_path.clone()
    } else {
        spec.work_dir.join(&exe_path)
    };
    let ro_binds = spec.ro_binds.clone();
    let jail_args: Vec<String> = spec
        .argv
        .iter()
        .skip(1)
        .map(|a| {
            let p = Path::new(a);
            if let Ok(rel) = p.strip_prefix(&work_dir) {
                return format!("/box/{}", rel.display());
            }
            a.to_string_lossy().to_string()
        })
        .collect();
    let jail_env: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().to_string(),
                v.to_string_lossy().to_string(),
            )
        })
        .collect();

    let cgroup_procs_file = cgroup.as_ref().map(|cg| cg.path().join("cgroup.procs"));
    let cgroup_mem_peak_file = cgroup.as_ref().map(|cg| cg.path().join("memory.peak"));
    let mem_limit_bytes = spec.limits.memory_bytes;
    let cpu_limit_ms = spec.limits.cpu_time_ms;
    let max_procs = spec.limits.max_processes;
    let output_limit_bytes = spec.limits.output_bytes;
    let has_cgroup = cgroup.is_some();

    let cpu_set_opt: Option<libc::cpu_set_t> = spec.cpu_affinity.as_ref().and_then(|cpus| {
        if cpus.is_empty() {
            return None;
        }
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            let mut any = false;
            for &cpu in cpus {
                if cpu < libc::CPU_SETSIZE as usize {
                    libc::CPU_SET(cpu, &mut set);
                    any = true;
                }
            }
            if any { Some(set) } else { None }
        }
    });

    let mut status_pipe = [0i32; 2];
    if unsafe { libc::pipe2(status_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    let status_read_owned = unsafe { OwnedFd::from_raw_fd(status_pipe[0]) };
    let status_write_owned = unsafe { OwnedFd::from_raw_fd(status_pipe[1]) };
    let status_write_fd = status_write_owned.as_raw_fd();

    // Pre-calculate all resources, CStrings, and file system skeletons in the parent thread
    // BEFORE fork, so the child running in pre_exec strictly executes async-signal-safe syscalls.
    let real_uid = unsafe { libc::getuid() };
    let real_gid = unsafe { libc::getgid() };
    // Map a non-zero UID/GID inside the user namespace so execve does not grant root capabilities.
    let inner_uid = if real_uid == 0 { 1000 } else { real_uid };
    let inner_gid = if real_gid == 0 { 1000 } else { real_gid };
    let uid_map_bytes = format!("{inner_uid} {real_uid} 1\n");
    let gid_map_bytes = format!("{inner_gid} {real_gid} 1\n");
    let cgroup_procs_cpath = cgroup_procs_file
        .as_ref()
        .and_then(|p| std::ffi::CString::new(p.to_string_lossy().as_bytes()).ok());
    let cgroup_mem_peak_cpath = cgroup_mem_peak_file
        .as_ref()
        .and_then(|p| std::ffi::CString::new(p.to_string_lossy().as_bytes()).ok());

    let jail_rootfs_guard: Option<tempfile::TempDir> = if used_namespaces {
        let base = if Path::new("/dev/shm").is_dir() {
            Path::new("/dev/shm")
        } else if Path::new("/run").is_dir() {
            Path::new("/run")
        } else {
            Path::new("/tmp")
        };
        let prefix = format!(".uj_rootfs_{}_", std::process::id());
        let td = tempfile::Builder::new()
            .prefix(&prefix)
            .rand_bytes(8)
            .tempdir_in(base)
            .or_else(|_| {
                tempfile::Builder::new()
                    .prefix(&prefix)
                    .rand_bytes(8)
                    .tempdir()
            })
            .map_err(SandboxError::Io)?;
        Some(td)
    } else {
        None
    };

    let (
        c_rootfs,
        c_usr,
        c_dev,
        c_proc,
        c_tmp,
        c_box,
        c_etc,
        c_oldroot,
        c_bin,
        c_lib,
        c_lib64,
        c_workdir,
        target_exe_pair,
        stdin_bind_pair,
        ro_bind_pairs,
        dev_pairs,
        ld_so_pair,
        alt_pair,
        c_argv_ptrs,
        c_env_ptrs,
        c_exe,
    ) = if let Some(ref rootfs_guard) = jail_rootfs_guard {
        let rootfs = rootfs_guard.path();

        let c_rootfs = std::ffi::CString::new(rootfs.to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid rootfs path: {e}")))?;
        let c_usr = std::ffi::CString::new(rootfs.join("usr").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid usr path: {e}")))?;
        let c_dev = std::ffi::CString::new(rootfs.join("dev").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid dev path: {e}")))?;
        let c_proc = std::ffi::CString::new(rootfs.join("proc").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid proc path: {e}")))?;
        let c_tmp = std::ffi::CString::new(rootfs.join("tmp").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid tmp path: {e}")))?;
        let c_box = std::ffi::CString::new(rootfs.join("box").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid box path: {e}")))?;
        let c_etc = std::ffi::CString::new(rootfs.join("etc").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid etc path: {e}")))?;
        let c_oldroot =
            std::ffi::CString::new(rootfs.join("old_root").to_string_lossy().as_bytes())
                .map_err(|e| SandboxError::Internal(format!("Invalid old_root path: {e}")))?;
        let c_bin = std::ffi::CString::new(rootfs.join("bin").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid bin path: {e}")))?;
        let c_lib = std::ffi::CString::new(rootfs.join("lib").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid lib path: {e}")))?;
        let c_lib64 = std::ffi::CString::new(rootfs.join("lib64").to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid lib64 path: {e}")))?;
        let c_workdir = std::ffi::CString::new(work_dir.to_string_lossy().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid workdir path: {e}")))?;

        let (jail_exe_path, target_exe_pair) =
            if host_exe.starts_with("/usr") || host_exe.starts_with("/bin") {
                (host_exe.to_string_lossy().to_string(), None)
            } else {
                let target_exe = rootfs.join("__uj_bin");
                let c_target_exe = std::ffi::CString::new(target_exe.to_string_lossy().as_bytes())
                    .map_err(|e| SandboxError::Internal(format!("Invalid target exe: {e}")))?;
                let c_host_exe = std::ffi::CString::new(host_exe.to_string_lossy().as_bytes())
                    .map_err(|e| SandboxError::Internal(format!("Invalid host exe: {e}")))?;
                ("/__uj_bin".to_string(), Some((c_host_exe, c_target_exe)))
            };

        let stdin_bind_pair = if let Some(ref host_in) = jail_stdin_host_path {
            let target_in = rootfs.join("__uj_stdin");
            let c_host_in = std::ffi::CString::new(host_in.to_string_lossy().as_bytes())
                .map_err(|e| SandboxError::Internal(format!("Invalid stdin path: {e}")))?;
            let c_target_in = std::ffi::CString::new(target_in.to_string_lossy().as_bytes())
                .map_err(|e| SandboxError::Internal(format!("Invalid target stdin: {e}")))?;
            Some((c_host_in, c_target_in))
        } else {
            None
        };

        let mut ro_bind_pairs = Vec::new();
        for ro_bind in &ro_binds {
            if ro_bind.exists() {
                let rel = ro_bind.strip_prefix("/").unwrap_or(ro_bind);
                let target = rootfs.join(rel);
                if let (Ok(c_src), Ok(c_dst)) = (
                    std::ffi::CString::new(ro_bind.to_string_lossy().as_bytes()),
                    std::ffi::CString::new(target.to_string_lossy().as_bytes()),
                ) {
                    ro_bind_pairs.push((c_src, c_dst, ro_bind.is_dir()));
                }
            }
        }

        let mut dev_pairs = Vec::new();
        for dev in ["null", "zero", "urandom", "random"] {
            let dst = rootfs.join("dev").join(dev);
            if let (Ok(c_src), Ok(c_dst)) = (
                std::ffi::CString::new(format!("/dev/{dev}")),
                std::ffi::CString::new(dst.to_string_lossy().as_bytes()),
            ) {
                dev_pairs.push((c_src, c_dst));
            }
        }

        let ld_so_pair = if Path::new("/etc/ld.so.cache").exists() {
            let dst_ld = rootfs.join("etc").join("ld.so.cache");
            if let (Ok(c_src), Ok(c_dst)) = (
                std::ffi::CString::new("/etc/ld.so.cache"),
                std::ffi::CString::new(dst_ld.to_string_lossy().as_bytes()),
            ) {
                Some((c_src, c_dst))
            } else {
                None
            }
        } else {
            None
        };

        let alt_pair = if Path::new("/etc/alternatives").is_dir() {
            let dst_alt = rootfs.join("etc").join("alternatives");
            if let (Ok(c_src), Ok(c_dst)) = (
                std::ffi::CString::new("/etc/alternatives"),
                std::ffi::CString::new(dst_alt.to_string_lossy().as_bytes()),
            ) {
                Some((c_src, c_dst))
            } else {
                None
            }
        } else {
            None
        };

        let mut jail_argv = Vec::new();
        jail_argv.push(jail_exe_path.clone());
        for arg in &jail_args {
            jail_argv.push(arg.clone());
        }
        let c_argv: Vec<std::ffi::CString> = jail_argv
            .iter()
            .map(|s| std::ffi::CString::new(s.as_bytes()).unwrap_or_default())
            .collect();
        let c_argv_ptrs = SendSyncPtrs::new(c_argv);

        let mut c_env: Vec<std::ffi::CString> = jail_env
            .iter()
            .map(|(k, v)| std::ffi::CString::new(format!("{k}={v}").as_bytes()).unwrap_or_default())
            .collect();
        if !jail_env.iter().any(|(k, _)| k == "PATH") {
            c_env.push(std::ffi::CString::new("PATH=/usr/bin:/bin").unwrap());
        }
        let c_env_ptrs = SendSyncPtrs::new(c_env);

        let c_exe = std::ffi::CString::new(jail_exe_path.as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid jail exe: {e}")))?;

        (
            Some(c_rootfs),
            Some(c_usr),
            Some(c_dev),
            Some(c_proc),
            Some(c_tmp),
            Some(c_box),
            Some(c_etc),
            Some(c_oldroot),
            Some(c_bin),
            Some(c_lib),
            Some(c_lib64),
            Some(c_workdir),
            target_exe_pair,
            stdin_bind_pair,
            ro_bind_pairs,
            dev_pairs,
            ld_so_pair,
            alt_pair,
            c_argv_ptrs,
            c_env_ptrs,
            Some(c_exe),
        )
    } else {
        (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
            SendSyncPtrs::empty(),
            SendSyncPtrs::empty(),
            None,
        )
    };

    // Build the seccomp BPF program in the parent (cached per process) so the forked
    // contestant only installs it; no allocation happens after fork.
    let seccomp_filter = crate::linux::seccomp::prebuilt_filter();

    unsafe {
        cmd.pre_exec(move || {
            // Raw async-signal-safe file writer using direct libc syscalls
            unsafe fn raw_write_all(
                path: *const libc::c_char,
                bytes: &[u8],
            ) -> std::io::Result<()> {
                let fd = unsafe { libc::open(path, libc::O_WRONLY) };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut written = 0;
                while written < bytes.len() {
                    let res = unsafe {
                        libc::write(
                            fd,
                            bytes[written..].as_ptr() as *const libc::c_void,
                            bytes.len() - written,
                        )
                    };
                    if res < 0 {
                        let err = std::io::Error::last_os_error();
                        unsafe {
                            libc::close(fd);
                        }
                        return Err(err);
                    }
                    written += res as usize;
                }
                unsafe {
                    libc::close(fd);
                }
                Ok(())
            }

            // Remount a bind-mounted path as read-only while preserving locked mount flags
            // (such as MS_NODEV, MS_NOSUID, MS_NOEXEC, MS_RELATIME/MS_NOATIME) required by userns.
            unsafe fn remount_bind_rdonly(
                target: *const libc::c_char,
                extra_flags: libc::c_ulong,
            ) -> std::io::Result<()> {
                let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
                let mut preserve: libc::c_ulong = 0;
                if unsafe { libc::statvfs(target, &mut st) } == 0 {
                    if (st.f_flag & libc::ST_NOSUID) != 0 {
                        preserve |= libc::MS_NOSUID;
                    }
                    if (st.f_flag & libc::ST_NODEV) != 0 {
                        preserve |= libc::MS_NODEV;
                    }
                    if (st.f_flag & libc::ST_NOEXEC) != 0 {
                        preserve |= libc::MS_NOEXEC;
                    }
                    if (st.f_flag & libc::ST_NOATIME) != 0 {
                        preserve |= libc::MS_NOATIME;
                    }
                    if (st.f_flag & libc::ST_NODIRATIME) != 0 {
                        preserve |= libc::MS_NODIRATIME;
                    }
                    if (st.f_flag & 4096/* ST_RELATIME */) != 0 {
                        preserve |= libc::MS_RELATIME;
                    }
                    if (st.f_flag & libc::ST_SYNCHRONOUS) != 0 {
                        preserve |= libc::MS_SYNCHRONOUS;
                    }
                }
                let flags =
                    preserve | libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | extra_flags;
                if unsafe {
                    libc::mount(
                        std::ptr::null(),
                        target,
                        std::ptr::null(),
                        flags,
                        std::ptr::null(),
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }

            // 1. Immediately join cgroup v2 before unsharing
            if let Some(ref procs_cpath) = cgroup_procs_cpath {
                raw_write_all(procs_cpath.as_ptr(), b"0\n")?;
            }

            // Open optional memory.peak fd (O_WRONLY | O_CLOEXEC) before pivot_root so PID 2
            // can attempt a kernel 6.12+ per-fd peak reset right before execve.
            let mem_peak_reset_fd = if let Some(ref peak_cpath) = cgroup_mem_peak_cpath {
                libc::open(peak_cpath.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC)
            } else {
                -1
            };

            if used_namespaces {
                // 1. Unshare user, pid, mount, net, ipc, uts namespaces
                if libc::unshare(
                    libc::CLONE_NEWUSER
                        | libc::CLONE_NEWPID
                        | libc::CLONE_NEWNS
                        | libc::CLONE_NEWNET
                        | libc::CLONE_NEWIPC
                        | libc::CLONE_NEWUTS,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // 2. Setup user namespace UID/GID mapping via raw syscalls
                raw_write_all(c"/proc/self/setgroups".as_ptr(), b"deny\n")?;
                raw_write_all(c"/proc/self/uid_map".as_ptr(), uid_map_bytes.as_bytes())?;
                raw_write_all(c"/proc/self/gid_map".as_ptr(), gid_map_bytes.as_bytes())?;

                // Mark intermediate process non-dumpable after writing uid_map/gid_map
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);

                // 3. Make all mounts private so rootfs mounts don't propagate to host
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                let c_rootfs_ptr = c_rootfs.as_ref().unwrap().as_ptr();
                let c_usr_ptr = c_usr.as_ref().unwrap().as_ptr();
                let c_dev_ptr = c_dev.as_ref().unwrap().as_ptr();
                let c_proc_ptr = c_proc.as_ref().unwrap().as_ptr();
                let c_tmp_ptr = c_tmp.as_ref().unwrap().as_ptr();
                let c_box_ptr = c_box.as_ref().unwrap().as_ptr();
                let c_etc_ptr = c_etc.as_ref().unwrap().as_ptr();
                let c_oldroot_ptr = c_oldroot.as_ref().unwrap().as_ptr();
                let c_bin_ptr = c_bin.as_ref().unwrap().as_ptr();
                let c_lib_ptr = c_lib.as_ref().unwrap().as_ptr();
                let c_lib64_ptr = c_lib64.as_ref().unwrap().as_ptr();
                let c_workdir_ptr = c_workdir.as_ref().unwrap().as_ptr();

                // 4. Mount private tmpfs rootfs
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_rootfs_ptr,
                    c"tmpfs".as_ptr(),
                    0,
                    c"size=64m".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Create directory skeleton on the fresh tmpfs using raw signal-safe mkdir syscalls
                for ptr in [
                    c_usr_ptr,
                    c_dev_ptr,
                    c_proc_ptr,
                    c_tmp_ptr,
                    c_box_ptr,
                    c_etc_ptr,
                    c_oldroot_ptr,
                ] {
                    if libc::mkdir(ptr, 0o755) != 0
                        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }

                // Symlinks on tmpfs
                let _ = libc::symlink(c"usr/bin".as_ptr(), c_bin_ptr);
                let _ = libc::symlink(c"usr/lib".as_ptr(), c_lib_ptr);
                let _ = libc::symlink(c"usr/lib64".as_ptr(), c_lib64_ptr);

                // Bind mount /usr (read-only)
                if libc::mount(
                    c"/usr".as_ptr(),
                    c_usr_ptr,
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                remount_bind_rdonly(c_usr_ptr, libc::MS_REC | libc::MS_NOSUID)?;

                // Bind mount /box (work_dir) read-write
                if libc::mount(
                    c_workdir_ptr,
                    c_box_ptr,
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Bind mount binary if outside /usr
                if let Some((ref c_host_exe, ref c_target_exe)) = target_exe_pair {
                    let fd =
                        libc::open(c_target_exe.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o755);
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    if libc::mount(
                        c_host_exe.as_ptr(),
                        c_target_exe.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    remount_bind_rdonly(c_target_exe.as_ptr(), libc::MS_NOSUID)?;
                }

                // Bind mount stdin input file read-only so /proc/self/fd/0 is backed by an MS_RDONLY mount
                if let Some((ref c_host_in, ref c_target_in)) = stdin_bind_pair {
                    let fd =
                        libc::open(c_target_in.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o444);
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    if libc::mount(
                        c_host_in.as_ptr(),
                        c_target_in.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    remount_bind_rdonly(c_target_in.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV)?;
                }

                // Ro binds
                for (c_src, c_dst, is_dir) in &ro_bind_pairs {
                    if *is_dir {
                        let _ = libc::mkdir(c_dst.as_ptr(), 0o755);
                    } else {
                        let fd = libc::open(c_dst.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
                        if fd >= 0 {
                            libc::close(fd);
                        }
                    }
                    if libc::mount(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    remount_bind_rdonly(c_dst.as_ptr(), libc::MS_REC | libc::MS_NOSUID)?;
                }

                // Private /tmp
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_tmp_ptr,
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    c"size=16m,mode=1777".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Private /dev
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_dev_ptr,
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID,
                    c"size=4m".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                for (c_src, c_dst) in &dev_pairs {
                    let fd = libc::open(c_dst.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o666);
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    let _ = libc::mount(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    );
                    let _ = remount_bind_rdonly(c_dst.as_ptr(), 0);
                }

                if let Some((ref c_src, ref c_dst)) = ld_so_pair {
                    let fd = libc::open(c_dst.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644);
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    let _ = libc::mount(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    );
                    let _ = remount_bind_rdonly(c_dst.as_ptr(), 0);
                }

                if let Some((ref c_src, ref c_dst)) = alt_pair {
                    let _ = libc::mkdir(c_dst.as_ptr(), 0o755);
                    let _ = libc::mount(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_RDONLY,
                        std::ptr::null(),
                    );
                }

                // 5. pivot_root
                if libc::syscall(libc::SYS_pivot_root, c_rootfs_ptr, c_oldroot_ptr) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::umount2(c"/old_root".as_ptr(), libc::MNT_DETACH) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = libc::rmdir(c"/old_root".as_ptr());

                // Open stdin from the read-only bind mount inside the jail, then detach and unlink it
                if stdin_bind_pair.is_some() {
                    let in_fd = libc::open(c"/__uj_stdin".as_ptr(), libc::O_RDONLY);
                    if in_fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if in_fd != 0 {
                        if libc::dup2(in_fd, 0) < 0 {
                            let err = std::io::Error::last_os_error();
                            libc::close(in_fd);
                            return Err(err);
                        }
                        libc::close(in_fd);
                    }
                    let _ = libc::umount2(c"/__uj_stdin".as_ptr(), libc::MNT_DETACH);
                    let _ = libc::unlink(c"/__uj_stdin".as_ptr());
                }

                // Remount rootfs as read-only, ensuring only /box and /tmp are writable
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                if libc::chdir(c"/box".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                // 6. Fork intermediate process so child enters new PID namespace as PID 1
                let init_pid = libc::fork();
                if init_pid < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                if init_pid == 0 {
                    // Child is now PID 1 in the new PID namespace!
                    // Mark PID 1 non-dumpable immediately so /proc/1/{fd,mem,maps,environ} are root-owned
                    // and inaccessible to the contestant (PID 2).
                    libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

                    let num_extra = extra_raw_fds.len().min(64);
                    let status_tx_fd = 3 + num_extra as i32;
                    let min_safe = status_tx_fd + 10;
                    let mut temp_fds = [0i32; 64];
                    for i in 0..num_extra {
                        let dup_fd = libc::fcntl(extra_raw_fds[i], libc::F_DUPFD_CLOEXEC, min_safe);
                        if dup_fd < 0 {
                            libc::_exit(124);
                        }
                        temp_fds[i] = dup_fd;
                    }
                    let temp_status_fd =
                        libc::fcntl(status_write_fd, libc::F_DUPFD_CLOEXEC, min_safe);
                    if temp_status_fd < 0 {
                        libc::_exit(124);
                    }
                    let temp_mem_peak_fd = if mem_peak_reset_fd >= 0 {
                        libc::fcntl(mem_peak_reset_fd, libc::F_DUPFD_CLOEXEC, min_safe)
                    } else {
                        -1
                    };
                    for (i, &tfd) in temp_fds.iter().enumerate().take(num_extra) {
                        let target_fd = 3 + i as i32;
                        if libc::dup2(tfd, target_fd) < 0 {
                            libc::_exit(124);
                        }
                        libc::close(tfd);
                    }
                    if libc::dup2(temp_status_fd, status_tx_fd) < 0 {
                        libc::_exit(124);
                    }
                    libc::close(temp_status_fd);
                    if status_write_fd > status_tx_fd {
                        libc::close(status_write_fd);
                    }
                    let peak_fd_target = status_tx_fd + 1;
                    let has_peak_fd = if temp_mem_peak_fd >= 0 {
                        if libc::dup2(temp_mem_peak_fd, peak_fd_target) >= 0 {
                            libc::fcntl(peak_fd_target, libc::F_SETFD, libc::FD_CLOEXEC);
                            libc::close(temp_mem_peak_fd);
                            true
                        } else {
                            libc::close(temp_mem_peak_fd);
                            false
                        }
                    } else {
                        false
                    };

                    // Close all fds > highest internal fd so parent Command::spawn() error pipe reaches EOF
                    let close_from = if has_peak_fd {
                        (peak_fd_target + 1) as u32
                    } else {
                        (status_tx_fd + 1) as u32
                    };
                    if libc::syscall(libc::SYS_close_range, close_from, !0u32, 0) != 0 {
                        for fd in (close_from as i32)..256 {
                            libc::close(fd);
                        }
                    }

                    if libc::mount(
                        c"proc".as_ptr(),
                        c"/proc".as_ptr(),
                        c"proc".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        c"subset=pid".as_ptr() as *const libc::c_void,
                    ) != 0
                    {
                        let _ = libc::mount(
                            c"proc".as_ptr(),
                            c"/proc".as_ptr(),
                            c"proc".as_ptr(),
                            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                            std::ptr::null(),
                        );
                    }

                    // Fork contestant process as PID 2
                    let contestant_pid = libc::fork();
                    if contestant_pid < 0 {
                        libc::_exit(126);
                    }

                    if contestant_pid == 0 {
                        libc::close(status_tx_fd);
                        if has_peak_fd {
                            // Best-effort reset of cgroup memory.peak (supported on Linux 6.12+)
                            let _ = libc::write(
                                peak_fd_target,
                                b"0\n".as_ptr() as *const libc::c_void,
                                2,
                            );
                            libc::close(peak_fd_target);
                        }
                        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

                        if let Some(ref set) = cpu_set_opt {
                            let _ = libc::sched_setaffinity(
                                0,
                                std::mem::size_of::<libc::cpu_set_t>(),
                                set,
                            );
                        }

                        let rlim = libc::rlimit {
                            rlim_cur: stack_limit,
                            rlim_max: stack_limit,
                        };
                        libc::setrlimit(libc::RLIMIT_STACK, &rlim);

                        let rlim_zero = libc::rlimit {
                            rlim_cur: 0,
                            rlim_max: 0,
                        };
                        libc::setrlimit(libc::RLIMIT_CORE, &rlim_zero);

                        // Always enforce RLIMIT_FSIZE regardless of tier (Full or Degraded)
                        // so contestant cannot fill host disk via /box writes.
                        if output_limit_bytes > 0 {
                            let rlim_fsize = libc::rlimit {
                                rlim_cur: output_limit_bytes,
                                rlim_max: output_limit_bytes,
                            };
                            libc::setrlimit(libc::RLIMIT_FSIZE, &rlim_fsize);
                        }

                        if tier == Tier::Degraded || !has_cgroup {
                            if mem_limit_bytes > 0 {
                                let rlim_as = libc::rlimit {
                                    rlim_cur: mem_limit_bytes,
                                    rlim_max: mem_limit_bytes,
                                };
                                libc::setrlimit(libc::RLIMIT_AS, &rlim_as);
                            }
                            if cpu_limit_ms > 0 {
                                let cpu_sec = cpu_limit_ms.div_ceil(1000);
                                let rlim_cpu = libc::rlimit {
                                    rlim_cur: cpu_sec,
                                    rlim_max: cpu_sec + 1,
                                };
                                libc::setrlimit(libc::RLIMIT_CPU, &rlim_cpu);
                            }
                            // Match cgroup.rs pids.max accounting: intermediate process + PID 1 + PID 2
                            // all share this user_namespace ucounts, plus a small buffer for runtime helpers.
                            let nproc = if max_procs > 0 {
                                max_procs.saturating_add(4)
                            } else {
                                32
                            };
                            let rlim_nproc = libc::rlimit {
                                rlim_cur: nproc as u64,
                                rlim_max: nproc as u64,
                            };
                            libc::setrlimit(libc::RLIMIT_NPROC, &rlim_nproc);
                        }

                        // Drop all capabilities (ambient, bounding set, effective/permitted/inheritable)
                        // and lock securebits before applying seccomp and execve.
                        let _ = libc::prctl(
                            libc::PR_CAP_AMBIENT,
                            libc::PR_CAP_AMBIENT_CLEAR_ALL,
                            0,
                            0,
                            0,
                        );
                        for cap in 0..=63 {
                            if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                                break;
                            }
                        }
                        let _ = libc::prctl(
                            libc::PR_SET_SECUREBITS,
                            SECUREBITS_LOCKED_NO_ROOT,
                            0,
                            0,
                            0,
                        );
                        let cap_hdr = CapUserHeader {
                            version: LINUX_CAPABILITY_VERSION_3,
                            pid: 0,
                        };
                        let cap_data = [CapUserData {
                            effective: 0,
                            permitted: 0,
                            inheritable: 0,
                        }; 2];
                        if libc::syscall(
                            libc::SYS_capset,
                            &cap_hdr as *const CapUserHeader,
                            cap_data.as_ptr(),
                        ) != 0
                        {
                            libc::_exit(125);
                        }

                        let seccomp_ok = match seccomp_filter {
                            Some(f) => crate::linux::seccomp::install_filter(f).is_ok(),
                            None => false,
                        };
                        if !seccomp_ok {
                            libc::_exit(125);
                        }

                        // Pure async-signal-safe execve using precomputed argv/env pointers
                        let c_exe_ptr = c_exe.as_ref().unwrap().as_ptr();
                        libc::execve(c_exe_ptr, c_argv_ptrs.as_ptr(), c_env_ptrs.as_ptr());
                        libc::_exit(127);
                    } else {
                        if has_peak_fd {
                            libc::close(peak_fd_target);
                        }
                        // PID 1 init process: wait for contestant PID 2, then reap any zombies
                        let mut status = 0;
                        let mut rusage: libc::rusage = std::mem::zeroed();
                        while libc::wait4(contestant_pid, &mut status, 0, &mut rusage) < 0 {
                            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                                break;
                            }
                        }
                        while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}

                        let is_signaled = if libc::WIFSIGNALED(status) { 1u8 } else { 0u8 };
                        let code_or_sig = if libc::WIFSIGNALED(status) {
                            libc::WTERMSIG(status)
                        } else if libc::WIFEXITED(status) {
                            libc::WEXITSTATUS(status)
                        } else {
                            1
                        };

                        let packet = SandboxStatusPacket {
                            is_signaled,
                            pad: [0; 3],
                            code_or_sig,
                            ru_utime_us: (rusage.ru_utime.tv_sec as u64) * 1_000_000
                                + (rusage.ru_utime.tv_usec as u64),
                            ru_stime_us: (rusage.ru_stime.tv_sec as u64) * 1_000_000
                                + (rusage.ru_stime.tv_usec as u64),
                            ru_maxrss_kb: rusage.ru_maxrss as u64,
                        };

                        let packet_bytes = std::slice::from_raw_parts(
                            &packet as *const _ as *const u8,
                            std::mem::size_of::<SandboxStatusPacket>(),
                        );
                        let mut written = 0;
                        while written < packet_bytes.len() {
                            let n = libc::write(
                                status_tx_fd,
                                packet_bytes[written..].as_ptr() as *const libc::c_void,
                                packet_bytes.len() - written,
                            );
                            if n <= 0 {
                                break;
                            }
                            written += n as usize;
                        }
                        libc::close(status_tx_fd);

                        if is_signaled != 0 {
                            libc::_exit(128 + code_or_sig);
                        } else {
                            libc::_exit(code_or_sig);
                        }
                    }
                } else {
                    // Close all fds >= 3 (including Rust's internal error reporting socket and status_write_fd)
                    if libc::syscall(libc::SYS_close_range, 3, !0u32, 0) != 0 {
                        for fd in 3..256 {
                            libc::close(fd);
                        }
                    }

                    libc::close(0);
                    libc::close(1);
                    libc::close(2);

                    let mut status = 0;
                    while libc::waitpid(init_pid, &mut status, 0) < 0 {
                        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                            break;
                        }
                    }
                    if libc::WIFEXITED(status) {
                        let code = libc::WEXITSTATUS(status);
                        if code > 128 && code <= 128 + 64 {
                            let sig = code - 128;
                            let mut set: libc::sigset_t = std::mem::zeroed();
                            libc::sigemptyset(&mut set);
                            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                            libc::signal(sig, libc::SIG_DFL);
                            libc::kill(libc::getpid(), sig);
                        }
                        libc::_exit(code);
                    } else if libc::WIFSIGNALED(status) {
                        let sig = libc::WTERMSIG(status);
                        let mut set: libc::sigset_t = std::mem::zeroed();
                        libc::sigemptyset(&mut set);
                        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                        libc::signal(sig, libc::SIG_DFL);
                        libc::kill(libc::getpid(), sig);
                        libc::_exit(128 + sig);
                    } else {
                        libc::_exit(1);
                    }
                }
            } else {
                // Trusted mode:
                if mem_peak_reset_fd >= 0 {
                    let _ =
                        libc::write(mem_peak_reset_fd, b"0\n".as_ptr() as *const libc::c_void, 2);
                    libc::close(mem_peak_reset_fd);
                }

                if let Some(ref set) = cpu_set_opt {
                    let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set);
                }

                let rlim = libc::rlimit {
                    rlim_cur: stack_limit,
                    rlim_max: stack_limit,
                };
                libc::setrlimit(libc::RLIMIT_STACK, &rlim);

                let rlim_zero = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                libc::setrlimit(libc::RLIMIT_CORE, &rlim_zero);

                let num_extra = extra_raw_fds.len().min(64);
                if num_extra > 0 {
                    let min_safe = 3 + num_extra as i32 + 10;
                    let mut temp_fds = [0i32; 64];
                    for i in 0..num_extra {
                        let dup_fd = libc::fcntl(extra_raw_fds[i], libc::F_DUPFD_CLOEXEC, min_safe);
                        if dup_fd < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        temp_fds[i] = dup_fd;
                    }
                    for (i, &tfd) in temp_fds.iter().enumerate().take(num_extra) {
                        let target_fd = 3 + i as i32;
                        if libc::dup2(tfd, target_fd) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        libc::close(tfd);
                    }
                }

                Ok(())
            }
        });
    }

    let start_time = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(SandboxError::Internal(format!(
                "Failed to spawn child: {e}"
            )));
        }
    };

    drop(extra_owned_fds);
    // Close supervisor's write end of the status pipe
    drop(status_write_owned);

    // 4. Attach child to cgroup v2 if available
    if let Some(ref cg) = cgroup {
        let _ = cg.add_process(child.id() as i32);
    }

    // 5. Output capture reader threads
    let stdout_limit = spec.limits.output_bytes as usize;
    let stderr_limit = spec.limits.stderr_bytes as usize;
    let ole_flag = Arc::new(AtomicBool::new(false));

    let stdout_handle = if capture_stdout {
        let mut stdout = child.stdout.take().unwrap();
        let ole = ole_flag.clone();
        Some(thread::spawn(move || {
            let mut buf = Vec::with_capacity(stdout_limit.min(65536));
            let mut chunk = vec![0u8; 65536];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() + n > stdout_limit {
                    let take = stdout_limit.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    ole.store(true, Ordering::SeqCst);
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            buf
        }))
    } else {
        None
    };

    let stderr_handle = {
        let mut stderr = child.stderr.take().unwrap();
        Some(thread::spawn(move || {
            let mut buf = Vec::with_capacity(stderr_limit.min(65536));
            let mut chunk = vec![0u8; 65536];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() < stderr_limit {
                    let take = (stderr_limit - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
                // Continue reading to EOF to drain stderr so child writing to stderr never blocks on a full pipe
            }
            buf
        }))
    };

    // 6. Monitor child process using pidfd_open or wait4 loop
    let pid = child.id() as i32;
    let pidfd = pidfd_open(pid).ok();
    let mut killed_by: Option<KillReason> = None;
    let mut waited_result: Option<(std::process::ExitStatus, libc::rusage)> = None;

    loop {
        if cancel.is_cancelled() {
            killed_by = Some(KillReason::UserCancel);
            if let Some(ref cg) = cgroup {
                cg.kill();
            }
            let _ = child.kill();
            break;
        }

        let wall_ms = start_time.elapsed().as_millis() as u64;
        if wall_ms > spec.limits.wall_time_ms {
            killed_by = Some(KillReason::WallLimit);
            if let Some(ref cg) = cgroup {
                cg.kill();
            }
            let _ = child.kill();
            break;
        }

        // Check CPU time limit
        if let Some(ref cg) = cgroup {
            let cpu_us = cg.read_cpu_time_us();
            if cpu_us / 1000 > spec.limits.cpu_time_ms {
                killed_by = Some(KillReason::CpuLimit);
                cg.kill();
                let _ = child.kill();
                break;
            }

            // Check Memory limit violation
            if cg.read_oom_killed() {
                killed_by = Some(KillReason::MemoryLimit);
                cg.kill();
                let _ = child.kill();
                break;
            }
        }

        // Check Output limit violation
        if ole_flag.load(Ordering::SeqCst) {
            killed_by = Some(KillReason::OutputLimit);
            if let Some(ref cg) = cgroup {
                cg.kill();
            }
            let _ = child.kill();
            break;
        }

        // Poll pidfd or wait4(WNOHANG)
        if let Some(pfd) = pidfd {
            let mut pfd_poll = libc::pollfd {
                fd: pfd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd_poll, 1, 10) };
            if ret > 0 && (pfd_poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                break;
            }
        } else {
            match wait4_pid(pid, libc::WNOHANG) {
                Ok(Some(res)) => {
                    waited_result = Some(res);
                    break;
                }
                Ok(None) => thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => break,
            }
        }
    }

    if let Some(pfd) = pidfd {
        unsafe {
            libc::close(pfd);
        }
    }

    let (status, wait_rusage) = match waited_result {
        Some(res) => res,
        None => match wait4_pid(pid, 0)? {
            Some(res) => res,
            None => (child.wait()?, unsafe { std::mem::zeroed() }),
        },
    };
    let wall_time_us = start_time.elapsed().as_micros() as u64;

    // Immediately kill any surviving child/orphan processes in this cgroup
    // so all pipe write ends close and reader threads can reach EOF.
    if let Some(ref cg) = cgroup {
        cg.kill();
    }

    // Read status packet from pipe: accept ONLY if exactly one packet followed by EOF.
    let status_from_packet = unsafe {
        const PKT_SIZE: usize = std::mem::size_of::<SandboxStatusPacket>();
        let mut raw_buf = [0u8; PKT_SIZE + 1];
        let mut read_bytes = 0;
        let status_read_fd = status_read_owned.as_raw_fd();
        while read_bytes < raw_buf.len() {
            let n = libc::read(
                status_read_fd,
                raw_buf[read_bytes..].as_mut_ptr() as *mut libc::c_void,
                raw_buf.len() - read_bytes,
            );
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            read_bytes += n as usize;
        }
        drop(status_read_owned);
        if read_bytes == PKT_SIZE {
            let mut packet = std::mem::zeroed::<SandboxStatusPacket>();
            std::ptr::copy_nonoverlapping(
                raw_buf.as_ptr(),
                &mut packet as *mut _ as *mut u8,
                PKT_SIZE,
            );
            if packet.is_signaled <= 1 && packet.pad == [0; 3] {
                Some(packet)
            } else {
                None
            }
        } else {
            None
        }
    };

    let (cpu_time_us, memory_peak_bytes, oom_killed) = if let Some(ref cg) = cgroup {
        (
            cg.read_cpu_time_us(),
            cg.read_peak_memory(),
            cg.read_oom_killed(),
        )
    } else if let Some(ref pkt) = status_from_packet {
        (
            pkt.ru_utime_us + pkt.ru_stime_us,
            pkt.ru_maxrss_kb * 1024,
            false,
        )
    } else {
        let cpu_us = (wait_rusage.ru_utime.tv_sec as u64) * 1_000_000
            + (wait_rusage.ru_utime.tv_usec as u64)
            + (wait_rusage.ru_stime.tv_sec as u64) * 1_000_000
            + (wait_rusage.ru_stime.tv_usec as u64);
        let mem_bytes = (wait_rusage.ru_maxrss as u64) * 1024;
        (cpu_us, mem_bytes, false)
    };

    // Join readers
    let captured_stdout = if let Some(h) = stdout_handle {
        let bytes = h.join().unwrap_or_default();
        let truncated = ole_flag.load(Ordering::SeqCst);
        Some(CapturedOutput { bytes, truncated })
    } else {
        None
    };

    let captured_stderr = if let Some(h) = stderr_handle {
        let bytes = h.join().unwrap_or_default();
        Some(CapturedOutput {
            bytes,
            truncated: false,
        })
    } else {
        None
    };

    // Nuclear kill and cleanup
    if let Some(ref cg) = cgroup {
        cg.kill();
    }
    drop(jail_rootfs_guard);

    let exit_status = if let Some(ref pkt) = status_from_packet {
        if pkt.is_signaled != 0 {
            ExitStatus::Signaled(pkt.code_or_sig)
        } else {
            ExitStatus::Exited(pkt.code_or_sig)
        }
    } else if let Some(sig) = status.signal() {
        ExitStatus::Signaled(sig)
    } else if let Some(code) = status.code() {
        if used_namespaces && code > 128 && code <= 128 + 64 {
            ExitStatus::Signaled(code - 128)
        } else {
            ExitStatus::Exited(code)
        }
    } else {
        ExitStatus::Signaled(0)
    };

    if (oom_killed
        || (spec.limits.memory_bytes > 0 && memory_peak_bytes >= spec.limits.memory_bytes))
        && killed_by.is_none()
    {
        killed_by = Some(KillReason::MemoryLimit);
    } else if spec.limits.cpu_time_ms > 0
        && cpu_time_us / 1000 > spec.limits.cpu_time_ms
        && killed_by.is_none()
    {
        killed_by = Some(KillReason::CpuLimit);
    } else if matches!(exit_status, ExitStatus::Signaled(24)) && killed_by.is_none() {
        // SIGXCPU
        killed_by = Some(KillReason::CpuLimit);
    } else if matches!(exit_status, ExitStatus::Signaled(25)) && killed_by.is_none() {
        // SIGXFSZ
        killed_by = Some(KillReason::OutputLimit);
    } else if captured_stdout.as_ref().is_some_and(|o| o.truncated) && killed_by.is_none() {
        killed_by = Some(KillReason::OutputLimit);
    }

    Ok(RunResult {
        status: exit_status,
        cpu_time_us,
        wall_time_us,
        memory_peak_bytes,
        oom_killed,
        killed_by,
        stdout: captured_stdout,
        stderr_tail: captured_stderr.map(|o| o.bytes).unwrap_or_default(),
        residue_cleaned: true,
        sandbox_tier: tier,
        exit_order: None,
    })
}

/// Executes a group of processes concurrently with wiring pipes between them.
pub fn execute_group_in_sandbox(
    specs: &[RunSpec],
    wiring: &Wiring,
    cgroup_base: Option<&Path>,
    cancel: &CancelToken,
    tier: Tier,
) -> Result<Vec<RunResult>, SandboxError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }

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
                m_to_p_write.as_raw_fd() as usize
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
    let results: Result<Vec<RunResult>, SandboxError> = thread::scope(|s| {
        let mut handles = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let in_fd = wired_stdins[i].take();
            let out_fd = wired_stdouts[i].take();
            let ex_fds = std::mem::take(&mut extra_owned[i]);
            handles.push(s.spawn(move || {
                execute_in_sandbox_with_stdio(
                    spec,
                    cgroup_base,
                    cancel,
                    tier,
                    in_fd,
                    out_fd,
                    ex_fds,
                )
            }));
        }

        let mut res = Vec::new();
        for h in handles {
            match h.join() {
                Ok(Ok(run_res)) => res.push(run_res),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(SandboxError::Internal(
                        "Thread panicked in execute_group".into(),
                    ));
                }
            }
        }
        Ok(res)
    });

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PipeConnection;
    use uj_core::run::Limits;

    fn count_open_fds() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|it| it.count())
            .unwrap_or(0)
    }

    #[test]
    fn test_execute_group_partial_error_no_fd_leak() {
        let temp_dir = std::env::temp_dir();
        let spec0 = RunSpec {
            argv: vec!["/bin/true".into()],
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
            argv: vec![], // Invalid empty argv causes immediate early error in worker
            env: vec![],
            work_dir: temp_dir,
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

        // Warm-up
        let _ = execute_group_in_sandbox(
            &[spec0.clone(), spec1.clone()],
            &wiring,
            None,
            &cancel,
            Tier::Degraded,
        );
        let before_fds = count_open_fds();

        for _ in 0..30 {
            let res = execute_group_in_sandbox(
                &[spec0.clone(), spec1.clone()],
                &wiring,
                None,
                &cancel,
                Tier::Degraded,
            );
            assert!(res.is_err());
        }

        let after_fds = count_open_fds();
        assert!(
            after_fds <= before_fds + 5,
            "FD leak detected! before={before_fds}, after={after_fds}"
        );
    }

    #[test]
    fn test_execute_group_duplicate_wiring_rejected() {
        let temp_dir = std::env::temp_dir();
        let spec = RunSpec {
            argv: vec!["/bin/true".into()],
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
        let res = execute_group_in_sandbox(
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

    #[test]
    fn test_trusted_exit_code_above_128_not_misreported_as_signal() {
        let temp_dir = std::env::temp_dir();
        let spec = RunSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "exit 130".into()],
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
        let cancel = CancelToken::new();
        let res = execute_in_sandbox(&spec, None, &cancel, Tier::Degraded).unwrap();
        assert_eq!(res.status, ExitStatus::Exited(130));
    }

    #[test]
    fn test_spawn_failure_cleans_up_rootfs_dir() {
        let temp_dir = std::env::temp_dir();
        let prefix = format!(".uj_rootfs_{}_", std::process::id());
        let count_rootfs = || -> usize {
            std::fs::read_dir("/dev/shm")
                .map(|it| {
                    it.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                        .count()
                })
                .unwrap_or(0)
        };
        let before = count_rootfs();
        let spec = RunSpec {
            argv: vec!["/nonexistent_binary_that_fails_spawn_12345".into()],
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
            trusted: false,
        };
        let cancel = CancelToken::new();
        let _ = execute_in_sandbox(&spec, None, &cancel, Tier::Degraded);
        let after = count_rootfs();
        assert_eq!(
            before, after,
            "Rootfs temp dir leaked in /dev/shm! before={before}, after={after}"
        );
    }
}

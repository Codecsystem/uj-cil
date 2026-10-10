//! Process runner and namespace isolation for Linux sandbox.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::process::CommandExt;
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

/// Helper to open a pidfd on Linux (syscall 434).
fn pidfd_open(pid: i32) -> Result<i32, std::io::Error> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

/// Executes a single process in Linux sandbox (cgroup v2 + namespaces).
pub fn execute_in_sandbox(
    spec: &RunSpec,
    cgroup_base: Option<&Path>,
    cancel: &CancelToken,
    tier: Tier,
) -> Result<RunResult, SandboxError> {
    execute_in_sandbox_with_stdio(spec, cgroup_base, cancel, tier, None, None)
}

/// Executes a single process in Linux sandbox with optional raw stdin/stdout fds.
pub fn execute_in_sandbox_with_stdio(
    spec: &RunSpec,
    cgroup_base: Option<&Path>,
    cancel: &CancelToken,
    tier: Tier,
    custom_stdin: Option<std::os::unix::io::RawFd>,
    custom_stdout: Option<std::os::unix::io::RawFd>,
) -> Result<RunResult, SandboxError> {
    use std::os::unix::io::FromRawFd;

    if spec.argv.is_empty() {
        return Err(SandboxError::Internal("Empty argv in RunSpec".to_string()));
    }

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
    if let Some(fd) = custom_stdin {
        cmd.stdin(unsafe { StdProcessStdio::from(File::from_raw_fd(fd)) });
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

    let capture_stdout = custom_stdout.is_none() && matches!(&spec.stdout, Stdio::Pipe);
    if let Some(fd) = custom_stdout {
        cmd.stdout(unsafe { StdProcessStdio::from(File::from_raw_fd(fd)) });
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
    let is_trusted = spec.trusted;

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
            if p.is_absolute() && p.starts_with(&work_dir) {
                if let Ok(rel) = p.strip_prefix(&work_dir) {
                    return format!("/box/{}", rel.display());
                }
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
    let mem_limit_bytes = spec.limits.memory_bytes;
    let cpu_limit_ms = spec.limits.cpu_time_ms;
    let max_procs = spec.limits.max_processes;
    let output_limit_bytes = spec.limits.output_bytes;
    let has_cgroup = cgroup.is_some();

    let mut status_pipe = [0i32; 2];
    if unsafe { libc::pipe2(status_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    let status_read_fd = status_pipe[0];
    let status_write_fd = status_pipe[1];

    unsafe {
        cmd.pre_exec(move || {
            // Immediately join the cgroup v2 before unsharing or execing
            if let Some(ref procs_file) = cgroup_procs_file {
                if fs::write(procs_file, b"0\n").is_err() {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if !is_trusted && (tier == Tier::Full || tier == Tier::Degraded) {
                // Get real UID and GID BEFORE unshare
                let real_uid = libc::getuid();
                let real_gid = libc::getgid();

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

                // 2. Setup user namespace UID/GID mapping
                fs::write("/proc/self/setgroups", b"deny\n")?;
                fs::write("/proc/self/uid_map", format!("0 {real_uid} 1\n").as_bytes())?;
                fs::write("/proc/self/gid_map", format!("0 {real_gid} 1\n").as_bytes())?;

                // 3. Make all mounts private so our rootfs mounts don't propagate to host
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

                // 4. Create minimal rootfs on a private tmpfs
                let rootfs = if work_dir == Path::new("/tmp") || work_dir.starts_with("/tmp") {
                    if Path::new("/dev/shm").is_dir() {
                        PathBuf::from(format!("/dev/shm/.uj_rootfs_{}", std::process::id()))
                    } else {
                        PathBuf::from(format!("/run/.uj_rootfs_{}", std::process::id()))
                    }
                } else {
                    work_dir.join(".uj_rootfs")
                };
                fs::create_dir_all(&rootfs)?;
                let c_rootfs = std::ffi::CString::new(rootfs.to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_rootfs.as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    c"size=64m".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                for d in ["usr", "dev", "proc", "tmp", "box", "etc", "old_root"] {
                    fs::create_dir_all(rootfs.join(d))?;
                }

                // Bind mount /usr (read-only)
                let c_usr = std::ffi::CString::new(rootfs.join("usr").to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::mount(
                    c"/usr".as_ptr(),
                    c_usr.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::mount(
                    std::ptr::null(),
                    c_usr.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND
                        | libc::MS_REMOUNT
                        | libc::MS_RDONLY
                        | libc::MS_REC
                        | libc::MS_NOSUID,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Symlink /bin, /lib, /lib64 to usr equivalents (standard Debian/Ubuntu)
                for (link_name, target) in [
                    ("bin", "usr/bin"),
                    ("lib", "usr/lib"),
                    ("lib64", "usr/lib64"),
                ] {
                    let link_path = rootfs.join(link_name);
                    let _ = std::os::unix::fs::symlink(target, &link_path);
                }

                // Bind mount /box (work_dir) read-write
                let c_box = std::ffi::CString::new(rootfs.join("box").to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let c_workdir = std::ffi::CString::new(work_dir.to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::mount(
                    c_workdir.as_ptr(),
                    c_box.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Bind mount binary if outside /usr
                let jail_exe_path = if host_exe.starts_with("/usr") || host_exe.starts_with("/bin")
                {
                    host_exe.to_string_lossy().to_string()
                } else {
                    let target_exe = rootfs.join("box").join("__uj_bin");
                    File::create(&target_exe)?;
                    let c_target_exe = std::ffi::CString::new(
                        target_exe.to_string_lossy().as_bytes(),
                    )
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                    let c_host_exe = std::ffi::CString::new(host_exe.to_string_lossy().as_bytes())
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
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
                    let _ = libc::mount(
                        std::ptr::null(),
                        c_target_exe.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                        std::ptr::null(),
                    );
                    "/box/__uj_bin".to_string()
                };

                // Ro binds
                for ro_bind in &ro_binds {
                    if ro_bind.exists() {
                        let rel = ro_bind.strip_prefix("/").unwrap_or(ro_bind);
                        let target = rootfs.join(rel);
                        if ro_bind.is_dir() {
                            let _ = fs::create_dir_all(&target);
                        } else {
                            if let Some(parent) = target.parent() {
                                let _ = fs::create_dir_all(parent);
                            }
                            let _ = File::create(&target);
                        }
                        if let Ok(c_src) =
                            std::ffi::CString::new(ro_bind.to_string_lossy().as_bytes())
                            && let Ok(c_dst) =
                                std::ffi::CString::new(target.to_string_lossy().as_bytes())
                        {
                            let _ = libc::mount(
                                c_src.as_ptr(),
                                c_dst.as_ptr(),
                                std::ptr::null(),
                                libc::MS_BIND,
                                std::ptr::null(),
                            );
                            let _ = libc::mount(
                                std::ptr::null(),
                                c_dst.as_ptr(),
                                std::ptr::null(),
                                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                                std::ptr::null(),
                            );
                        }
                    }
                }

                // Private /tmp
                let c_tmp = std::ffi::CString::new(rootfs.join("tmp").to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_tmp.as_ptr(),
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    c"size=16m,mode=1777".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                // Private /dev
                let c_dev = std::ffi::CString::new(rootfs.join("dev").to_string_lossy().as_bytes())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_dev.as_ptr(),
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID,
                    c"size=4m".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                for dev in ["null", "zero", "urandom", "random"] {
                    let dst = rootfs.join("dev").join(dev);
                    let _ = File::create(&dst);
                    if let Ok(c_dst) = std::ffi::CString::new(dst.to_string_lossy().as_bytes())
                        && let Ok(c_src) = std::ffi::CString::new(format!("/dev/{dev}"))
                    {
                        let _ = libc::mount(
                            c_src.as_ptr(),
                            c_dst.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND,
                            std::ptr::null(),
                        );
                        let _ = libc::mount(
                            std::ptr::null(),
                            c_dst.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                            std::ptr::null(),
                        );
                    }
                }

                // /etc/ld.so.cache and alternatives
                if Path::new("/etc/ld.so.cache").exists() {
                    let dst_ld = rootfs.join("etc").join("ld.so.cache");
                    let _ = File::create(&dst_ld);
                    if let Ok(c_dst) = std::ffi::CString::new(dst_ld.to_string_lossy().as_bytes()) {
                        let _ = libc::mount(
                            c"/etc/ld.so.cache".as_ptr(),
                            c_dst.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND,
                            std::ptr::null(),
                        );
                        let _ = libc::mount(
                            std::ptr::null(),
                            c_dst.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                            std::ptr::null(),
                        );
                    }
                }
                if Path::new("/etc/alternatives").is_dir() {
                    let dst_alt = rootfs.join("etc").join("alternatives");
                    let _ = fs::create_dir_all(&dst_alt);
                    if let Ok(c_dst) = std::ffi::CString::new(dst_alt.to_string_lossy().as_bytes())
                    {
                        let _ = libc::mount(
                            c"/etc/alternatives".as_ptr(),
                            c_dst.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND | libc::MS_RDONLY,
                            std::ptr::null(),
                        );
                    }
                }

                // 5. pivot_root
                let c_oldroot =
                    std::ffi::CString::new(rootfs.join("old_root").to_string_lossy().as_bytes())
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                if libc::syscall(libc::SYS_pivot_root, c_rootfs.as_ptr(), c_oldroot.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::umount2(c"/old_root".as_ptr(), libc::MNT_DETACH) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = fs::remove_dir("/old_root");

                // Remount rootfs as read-only, ensuring only /box and /tmp are writable
                let _ = libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                );

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
                    // Automatically kill PID 1 if intermediate host process dies
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

                    let status_tx_fd = 3;
                    if status_write_fd != status_tx_fd {
                        if libc::dup2(status_write_fd, status_tx_fd) < 0 {
                            libc::_exit(124);
                        }
                        libc::close(status_write_fd);
                    }

                    // Close all fds >= 4 so parent Command::spawn() error pipe reaches EOF
                    if libc::syscall(libc::SYS_close_range, 4, !0u32, 0) != 0 {
                        for fd in 4..256 {
                            libc::close(fd);
                        }
                    }

                    if libc::mount(
                        c"proc".as_ptr(),
                        c"/proc".as_ptr(),
                        c"proc".as_ptr(),
                        0,
                        c"subset=pid".as_ptr() as *const libc::c_void,
                    ) != 0
                    {
                        let _ = libc::mount(
                            c"proc".as_ptr(),
                            c"/proc".as_ptr(),
                            c"proc".as_ptr(),
                            0,
                            std::ptr::null(),
                        );
                    }

                    // Fork contestant process as PID 2 (normal process, standard signal semantics)
                    let contestant_pid = libc::fork();
                    if contestant_pid < 0 {
                        libc::_exit(126);
                    }

                    if contestant_pid == 0 {
                        libc::close(status_tx_fd);
                        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

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

                        if tier == Tier::Degraded || !has_cgroup {
                            if mem_limit_bytes > 0 {
                                let rlim_as = libc::rlimit {
                                    rlim_cur: mem_limit_bytes,
                                    rlim_max: mem_limit_bytes,
                                };
                                libc::setrlimit(libc::RLIMIT_AS, &rlim_as);
                            }
                            if cpu_limit_ms > 0 {
                                let cpu_sec = (cpu_limit_ms + 999) / 1000;
                                let rlim_cpu = libc::rlimit {
                                    rlim_cur: cpu_sec,
                                    rlim_max: cpu_sec + 1,
                                };
                                libc::setrlimit(libc::RLIMIT_CPU, &rlim_cpu);
                            }
                            let nproc = if max_procs > 0 { max_procs } else { 32 };
                            let rlim_nproc = libc::rlimit {
                                rlim_cur: nproc as u64,
                                rlim_max: nproc as u64,
                            };
                            libc::setrlimit(libc::RLIMIT_NPROC, &rlim_nproc);
                            if output_limit_bytes > 0 {
                                let rlim_fsize = libc::rlimit {
                                    rlim_cur: output_limit_bytes,
                                    rlim_max: output_limit_bytes,
                                };
                                libc::setrlimit(libc::RLIMIT_FSIZE, &rlim_fsize);
                            }
                        }

                        if crate::linux::seccomp::apply_seccomp_filter().is_err() {
                            libc::_exit(125);
                        }

                        let mut jail_argv = Vec::new();
                        jail_argv.push(jail_exe_path.clone());
                        for arg in &jail_args {
                            jail_argv.push(arg.clone());
                        }
                        let c_argv: Vec<std::ffi::CString> = jail_argv
                            .iter()
                            .map(|s| std::ffi::CString::new(s.as_bytes()).unwrap_or_default())
                            .collect();
                        let mut c_argv_ptrs: Vec<*const libc::c_char> =
                            c_argv.iter().map(|s| s.as_ptr()).collect();
                        c_argv_ptrs.push(std::ptr::null());

                        let mut c_env: Vec<std::ffi::CString> = jail_env
                            .iter()
                            .map(|(k, v)| {
                                std::ffi::CString::new(format!("{k}={v}").as_bytes())
                                    .unwrap_or_default()
                            })
                            .collect();
                        if !jail_env.iter().any(|(k, _)| k == "PATH") {
                            c_env.push(std::ffi::CString::new("PATH=/usr/bin:/bin").unwrap());
                        }
                        let mut c_env_ptrs: Vec<*const libc::c_char> =
                            c_env.iter().map(|s| s.as_ptr()).collect();
                        c_env_ptrs.push(std::ptr::null());

                        let c_exe = std::ffi::CString::new(jail_exe_path.as_bytes()).unwrap();
                        libc::execve(c_exe.as_ptr(), c_argv_ptrs.as_ptr(), c_env_ptrs.as_ptr());
                        libc::_exit(127);
                    } else {
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
                // Trusted or Degraded mode:
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

                Ok(())
            }
        });
    }

    let start_time = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(status_read_fd);
                libc::close(status_write_fd);
            }
            return Err(SandboxError::Internal(format!(
                "Failed to spawn child: {e}"
            )));
        }
    };

    // Close supervisor's write end of the status pipe
    unsafe {
        libc::close(status_write_fd);
    }

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
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
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
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if buf.len() + n > stderr_limit {
                    let take = stderr_limit.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            buf
        }))
    };

    // 6. Monitor child process using pidfd_open or wait loop
    let pid = child.id() as i32;
    let pidfd = pidfd_open(pid).ok();
    let mut killed_by: Option<KillReason> = None;

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

        // Poll pidfd or try_wait
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
            match child.try_wait() {
                Ok(Some(_)) => break,
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

    let status = child.wait()?;
    let wall_time_us = start_time.elapsed().as_micros() as u64;

    // Immediately kill any surviving child/orphan processes in this cgroup
    // so all pipe write ends close and reader threads can reach EOF.
    if let Some(ref cg) = cgroup {
        cg.kill();
    }

    // Read status packet from pipe
    let status_from_packet = unsafe {
        let mut packet = std::mem::zeroed::<SandboxStatusPacket>();
        let mut read_bytes = 0;
        let packet_buf = std::slice::from_raw_parts_mut(
            &mut packet as *mut _ as *mut u8,
            std::mem::size_of::<SandboxStatusPacket>(),
        );
        while read_bytes < packet_buf.len() {
            let n = libc::read(
                status_read_fd,
                packet_buf[read_bytes..].as_mut_ptr() as *mut libc::c_void,
                packet_buf.len() - read_bytes,
            );
            if n <= 0 {
                break;
            }
            read_bytes += n as usize;
        }
        libc::close(status_read_fd);
        if read_bytes == std::mem::size_of::<SandboxStatusPacket>() {
            Some(packet)
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
        (wall_time_us, 0, false)
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

    let exit_status = if let Some(ref pkt) = status_from_packet {
        if pkt.is_signaled != 0 {
            ExitStatus::Signaled(pkt.code_or_sig)
        } else {
            ExitStatus::Exited(pkt.code_or_sig)
        }
    } else if let Some(code) = status.code() {
        if code > 128 && code <= 128 + 64 {
            ExitStatus::Signaled(code - 128)
        } else {
            ExitStatus::Exited(code)
        }
    } else {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::Signaled(status.signal().unwrap_or(0))
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
    let mut wired_stdins: Vec<Option<std::os::unix::io::RawFd>> = vec![None; n];
    let mut wired_stdouts: Vec<Option<std::os::unix::io::RawFd>> = vec![None; n];

    for conn in &wiring.connections {
        if conn.from_proc >= n || conn.to_proc >= n {
            return Err(SandboxError::Internal(format!(
                "Invalid wiring connection from {} to {} (total procs: {})",
                conn.from_proc, conn.to_proc, n
            )));
        }
        let mut fds = [0i32; 2];
        let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if res != 0 {
            return Err(SandboxError::Io(std::io::Error::last_os_error()));
        }
        wired_stdouts[conn.from_proc] = Some(fds[1]);
        wired_stdins[conn.to_proc] = Some(fds[0]);
    }

    // Run specs concurrently in worker threads
    let results: Result<Vec<RunResult>, SandboxError> = thread::scope(|s| {
        let mut handles = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let in_fd = wired_stdins[i];
            let out_fd = wired_stdouts[i];
            handles.push(s.spawn(move || {
                execute_in_sandbox_with_stdio(spec, cgroup_base, cancel, tier, in_fd, out_fd)
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

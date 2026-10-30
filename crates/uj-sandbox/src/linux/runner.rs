//! Process runner and namespace isolation for Linux sandbox.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::Instant;

use crate::linux::cgroup::{CgroupV2, DIRECT_HELPER_PROCS, LEGACY_HELPER_PROCS, parse_usage_usec};
use crate::{CancelToken, SandboxError, Wiring};
use uj_core::run::{CapturedOutput, ExitStatus, KillReason, RunResult, RunSpec, Stdio, Tier};

/// Packet sent from container init (PID 1) to supervisor process over anonymous pipe.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SandboxStatusPacket {
    is_signaled: u8,
    has_cgroup_cpu: u8,
    pad: [u8; 2],
    code_or_sig: i32,
    ru_utime_us: u64,
    ru_stime_us: u64,
    ru_maxrss_kb: u64,
    contestant_cpu_us: u64,
    contestant_wall_us: u64,
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

#[repr(C)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

#[repr(C, align(16))]
struct VforkStack([u8; 32 * 1024]);

const LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;
// SECBIT_NOROOT (1) | SECBIT_NOROOT_LOCKED (2) | SECBIT_NO_SETUID_FIXUP (4)
// | SECBIT_NO_SETUID_FIXUP_LOCKED (8) | SECBIT_KEEP_CAPS_LOCKED (32) = 0x2f
const SECUREBITS_LOCKED_NO_ROOT: libc::c_ulong = 0x2f;

const CLONE_PIDFD_FLAG: u64 = 0x0000_1000;
const CLONE_INTO_CGROUP_FLAG: u64 = 0x2_0000_0000;

const SYS_OPEN_TREE_NUM: libc::c_long = 428;
const SYS_MOVE_MOUNT_NUM: libc::c_long = 429;
const SYS_CLONE3_NUM: libc::c_long = 435;
const SYS_MOUNT_SETATTR_NUM: libc::c_long = 442;

const OPEN_TREE_CLONE_FLAG: libc::c_uint = 1;
const OPEN_TREE_CLOEXEC_FLAG: libc::c_uint = libc::O_CLOEXEC as libc::c_uint;
const MOVE_MOUNT_F_EMPTY_PATH_FLAG: libc::c_uint = 0x0000_0004;
const AT_RECURSIVE_FLAG: libc::c_int = 0x8000;

const MOUNT_ATTR_RDONLY_FLAG: u64 = 0x0000_0001;
const MOUNT_ATTR_NOSUID_FLAG: u64 = 0x0000_0002;
const MOUNT_ATTR_NODEV_FLAG: u64 = 0x0000_0004;

#[derive(Clone)]
struct SendSyncPtrs {
    _storage: Vec<CString>,
    ptrs: Vec<*const libc::c_char>,
}
unsafe impl Send for SendSyncPtrs {}
unsafe impl Sync for SendSyncPtrs {}

impl SendSyncPtrs {
    pub fn new(cstrings: Vec<CString>) -> Self {
        let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|s| s.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Self {
            _storage: cstrings,
            ptrs,
        }
    }

    pub fn as_ptr(&self) -> *const *const libc::c_char {
        self.ptrs.as_ptr()
    }
}

/// Process-wide cache for `which::which` lookups on bare executable names.
fn resolve_executable(arg0: &OsStr) -> PathBuf {
    if arg0.as_bytes().contains(&b'/') {
        return PathBuf::from(arg0);
    }
    static WHICH_CACHE: OnceLock<parking_lot::Mutex<HashMap<OsString, PathBuf>>> = OnceLock::new();
    let cache = WHICH_CACHE.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    if let Some(cached) = cache.lock().get(arg0) {
        return cached.clone();
    }
    if let Ok(resolved) = which::which(arg0) {
        cache.lock().insert(arg0.to_os_string(), resolved.clone());
        resolved
    } else {
        PathBuf::from(arg0)
    }
}

fn host_has_ld_so_cache() -> bool {
    static HAS_LD_CACHE: OnceLock<bool> = OnceLock::new();
    *HAS_LD_CACHE.get_or_init(|| Path::new("/etc/ld.so.cache").exists())
}

fn host_has_etc_alternatives() -> bool {
    static HAS_ALT: OnceLock<bool> = OnceLock::new();
    *HAS_ALT.get_or_init(|| Path::new("/etc/alternatives").is_dir())
}

fn rootfs_base_dir() -> &'static Path {
    static BASE: OnceLock<&'static Path> = OnceLock::new();
    BASE.get_or_init(|| {
        if Path::new("/dev/shm").is_dir() {
            Path::new("/dev/shm")
        } else if Path::new("/run").is_dir() {
            Path::new("/run")
        } else {
            Path::new("/tmp")
        }
    })
}

/// Removes the temporary mountpoint directory after child teardown, retrying briefly on `EBUSY`
/// while the kernel finishes asynchronous mount-namespace release (`put_mnt_ns`).
fn cleanup_rootfs_dir(guard: Option<tempfile::TempDir>) {
    let Some(td) = guard else {
        return;
    };
    let path = td.keep();
    let mut delay_us = 100u64;
    for _ in 0..16 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {}
        }
        thread::sleep(std::time::Duration::from_micros(delay_us));
        delay_us = (delay_us * 2).min(5_000);
    }
}

fn dev_null_rd() -> Result<i32, SandboxError> {
    static DEV_NULL_R: OnceLock<Option<File>> = OnceLock::new();
    let opt = DEV_NULL_R.get_or_init(|| File::open("/dev/null").ok());
    opt.as_ref().map(AsRawFd::as_raw_fd).ok_or_else(|| {
        SandboxError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Failed to open /dev/null for reading",
        ))
    })
}

fn dev_null_wr() -> Result<i32, SandboxError> {
    static DEV_NULL_W: OnceLock<Option<File>> = OnceLock::new();
    let opt = DEV_NULL_W.get_or_init(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .ok()
    });
    opt.as_ref().map(AsRawFd::as_raw_fd).ok_or_else(|| {
        SandboxError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Failed to open /dev/null for writing",
        ))
    })
}

fn make_cloexec_pipe() -> Result<(OwnedFd, OwnedFd), SandboxError> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    let r = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let w = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((r, w))
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Helper to open a pidfd on Linux (syscall 434).
fn pidfd_open(pid: i32) -> Result<OwnedFd, std::io::Error> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
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

#[inline]
unsafe fn raw_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

/// Raw async-signal-safe file writer using direct libc syscalls.
unsafe fn raw_write_all(path: *const libc::c_char, bytes: &[u8]) -> Result<(), i32> {
    let fd = unsafe { libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(unsafe { raw_errno() });
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
            let err = unsafe { raw_errno() };
            if err == libc::EINTR {
                continue;
            }
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

unsafe fn report_spawn_errno_and_exit(err_fd: i32, errno: i32) -> ! {
    if err_fd >= 0 {
        let bytes = errno.to_ne_bytes();
        let _ = unsafe { libc::write(err_fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        unsafe {
            libc::close(err_fd);
        }
    }
    unsafe {
        libc::_exit(126);
    }
}

unsafe fn close_fds_from(first_fd: u32) {
    if unsafe { libc::syscall(libc::SYS_close_range, first_fd, !0u32, 0) } != 0 {
        for fd in (first_fd as i32)..1024 {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// Remount a bind-mounted path as read-only while preserving locked mount flags
/// (such as MS_NODEV, MS_NOSUID, MS_NOEXEC, MS_RELATIME/MS_NOATIME) required by userns.
unsafe fn remount_bind_rdonly(
    target: *const libc::c_char,
    extra_flags: libc::c_ulong,
) -> Result<(), i32> {
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
    let flags = preserve | libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | extra_flags;
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
        return Err(unsafe { raw_errno() });
    }
    Ok(())
}

/// Bind-mounts `src` onto `dst` read-only (recursively when `recursive` is true).
///
/// Prefers Linux 5.12+ `open_tree(OPEN_TREE_CLONE)` + `mount_setattr(AT_RECURSIVE)` + `move_mount`,
/// falling back to `mount(MS_BIND)` + `mount_setattr(AT_RECURSIVE)` (which still recursively locks
/// submounts read-only), and finally to legacy `remount_bind_rdonly` on older kernels.
unsafe fn bind_mount_ro(
    src: *const libc::c_char,
    dst: *const libc::c_char,
    recursive: bool,
    extra_mount_attr: u64,
    legacy_extra_ms: libc::c_ulong,
) -> Result<(), i32> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY_FLAG | extra_mount_attr,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };

    let open_flags = OPEN_TREE_CLONE_FLAG
        | OPEN_TREE_CLOEXEC_FLAG
        | if recursive {
            AT_RECURSIVE_FLAG as libc::c_uint
        } else {
            0
        };
    let tree_fd =
        unsafe { libc::syscall(SYS_OPEN_TREE_NUM, libc::AT_FDCWD, src, open_flags) as i32 };
    if tree_fd >= 0 {
        let setattr_flags = libc::AT_EMPTY_PATH | if recursive { AT_RECURSIVE_FLAG } else { 0 };
        let set_rc = unsafe {
            libc::syscall(
                SYS_MOUNT_SETATTR_NUM,
                tree_fd,
                c"".as_ptr(),
                setattr_flags,
                &attr as *const MountAttr,
                std::mem::size_of::<MountAttr>(),
            )
        };
        if set_rc == 0 {
            let move_rc = unsafe {
                libc::syscall(
                    SYS_MOVE_MOUNT_NUM,
                    tree_fd,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    dst,
                    MOVE_MOUNT_F_EMPTY_PATH_FLAG,
                )
            };
            if move_rc == 0 {
                unsafe {
                    libc::close(tree_fd);
                }
                return Ok(());
            }
        }
        unsafe {
            libc::close(tree_fd);
        }
    }

    let bind_flags = libc::MS_BIND | if recursive { libc::MS_REC } else { 0 };
    if unsafe { libc::mount(src, dst, std::ptr::null(), bind_flags, std::ptr::null()) } != 0 {
        return Err(unsafe { raw_errno() });
    }

    let setattr_flags = libc::AT_SYMLINK_NOFOLLOW | if recursive { AT_RECURSIVE_FLAG } else { 0 };
    let set_rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR_NUM,
            libc::AT_FDCWD,
            dst,
            setattr_flags,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if set_rc == 0 {
        return Ok(());
    }

    unsafe { remount_bind_rdonly(dst, legacy_extra_ms) }
}

#[inline]
unsafe fn monotonic_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    } else {
        0
    }
}

#[inline]
unsafe fn read_cpu_stat_fd_us(fd: i32) -> Option<u64> {
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 512];
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n <= 0 {
        return None;
    }
    parse_usage_usec(&buf[..(n as usize)])
}

/// Context passed into the `CLONE_VM | CLONE_VFORK` contestant child (PID 2).
struct ContestantVforkCtx {
    status_tx_fd: i32,
    cpu_stat_fd: i32,
    cpu_set_opt: Option<libc::cpu_set_t>,
    stack_limit: u64,
    output_limit_bytes: u64,
    degraded_or_no_cgroup: bool,
    mem_limit_bytes: u64,
    cpu_limit_ms: u64,
    max_procs: u32,
    seccomp_filter: Option<&'static [crate::linux::seccomp::sock_filter]>,
    c_exe_ptr: *const libc::c_char,
    c_argv_ptr: *const *const libc::c_char,
    c_env_ptr: *const *const libc::c_char,
}

unsafe fn run_contestant_exec(ctx: &ContestantVforkCtx) -> ! {
    unsafe {
        if ctx.status_tx_fd >= 0 {
            libc::close(ctx.status_tx_fd);
        }
        if ctx.cpu_stat_fd >= 0 {
            libc::close(ctx.cpu_stat_fd);
        }
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

        if let Some(ref set) = ctx.cpu_set_opt {
            let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set);
        }

        let rlim = libc::rlimit {
            rlim_cur: ctx.stack_limit,
            rlim_max: ctx.stack_limit,
        };
        libc::setrlimit(libc::RLIMIT_STACK, &rlim);

        let rlim_zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &rlim_zero);

        // Always enforce RLIMIT_FSIZE regardless of tier (Full or Degraded)
        // so contestant cannot fill host disk via /box writes.
        if ctx.output_limit_bytes > 0 {
            let rlim_fsize = libc::rlimit {
                rlim_cur: ctx.output_limit_bytes,
                rlim_max: ctx.output_limit_bytes,
            };
            libc::setrlimit(libc::RLIMIT_FSIZE, &rlim_fsize);
        }

        if ctx.degraded_or_no_cgroup {
            if ctx.mem_limit_bytes > 0 {
                let rlim_as = libc::rlimit {
                    rlim_cur: ctx.mem_limit_bytes,
                    rlim_max: ctx.mem_limit_bytes,
                };
                libc::setrlimit(libc::RLIMIT_AS, &rlim_as);
            }
            if ctx.cpu_limit_ms > 0 {
                let cpu_sec = ctx.cpu_limit_ms.div_ceil(1000);
                let rlim_cpu = libc::rlimit {
                    rlim_cur: cpu_sec,
                    rlim_max: cpu_sec + 1,
                };
                libc::setrlimit(libc::RLIMIT_CPU, &rlim_cpu);
            }
            let nproc = if ctx.max_procs > 0 {
                ctx.max_procs.saturating_add(4)
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
        let _ = libc::prctl(libc::PR_SET_SECUREBITS, SECUREBITS_LOCKED_NO_ROOT, 0, 0, 0);
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

        let seccomp_ok = match ctx.seccomp_filter {
            Some(f) => crate::linux::seccomp::install_filter(f).is_ok(),
            None => false,
        };
        if !seccomp_ok {
            libc::_exit(125);
        }

        libc::execve(ctx.c_exe_ptr, ctx.c_argv_ptr, ctx.c_env_ptr);
        libc::_exit(127);
    }
}

extern "C" fn contestant_vfork_trampoline(arg: *mut libc::c_void) -> libc::c_int {
    let ctx = unsafe { &*(arg as *const ContestantVforkCtx) };
    unsafe { run_contestant_exec(ctx) }
}

/// Context passed into the `CLONE_VM | CLONE_VFORK` trusted child (compilers/checkers/validators).
struct TrustedVforkCtx {
    stdin_fd: i32,
    stdout_fd: i32,
    stderr_fd: i32,
    extra_fds: [i32; 64],
    num_extra: usize,
    err_write_fd: i32,
    cgroup_procs_ptr: *const libc::c_char,
    mem_peak_fd: i32,
    c_workdir_ptr: *const libc::c_char,
    cpu_set_opt: Option<libc::cpu_set_t>,
    stack_limit: u64,
    c_exe_ptr: *const libc::c_char,
    c_argv_ptr: *const *const libc::c_char,
    c_env_ptr: *const *const libc::c_char,
}

extern "C" fn trusted_vfork_trampoline(arg: *mut libc::c_void) -> libc::c_int {
    unsafe {
        let ctx = &*(arg as *const TrustedVforkCtx);

        let min_safe = 3 + (ctx.num_extra as i32) + 16;
        let dup_safe = |fd: i32| -> Result<i32, i32> {
            let d = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min_safe);
            if d < 0 { Err(raw_errno()) } else { Ok(d) }
        };

        let tmp_err = match dup_safe(ctx.err_write_fd) {
            Ok(fd) => fd,
            Err(_) => libc::_exit(126),
        };
        let fail = |errno: i32| -> ! { report_spawn_errno_and_exit(tmp_err, errno) };

        if !ctx.cgroup_procs_ptr.is_null()
            && let Err(e) = raw_write_all(ctx.cgroup_procs_ptr, b"0\n")
        {
            fail(e);
        }
        if ctx.mem_peak_fd >= 0 {
            let _ = libc::write(ctx.mem_peak_fd, b"0\n".as_ptr() as *const libc::c_void, 2);
        }
        if libc::chdir(ctx.c_workdir_ptr) != 0 {
            fail(raw_errno());
        }

        let tmp_in = match dup_safe(ctx.stdin_fd) {
            Ok(fd) => fd,
            Err(e) => fail(e),
        };
        let tmp_out = match dup_safe(ctx.stdout_fd) {
            Ok(fd) => fd,
            Err(e) => fail(e),
        };
        let tmp_stderr = match dup_safe(ctx.stderr_fd) {
            Ok(fd) => fd,
            Err(e) => fail(e),
        };
        let mut tmp_extra = [0i32; 64];
        for (slot, &src_fd) in tmp_extra
            .iter_mut()
            .zip(ctx.extra_fds.iter())
            .take(ctx.num_extra)
        {
            *slot = match dup_safe(src_fd) {
                Ok(fd) => fd,
                Err(e) => fail(e),
            };
        }

        if libc::dup2(tmp_in, 0) < 0 || libc::dup2(tmp_out, 1) < 0 || libc::dup2(tmp_stderr, 2) < 0
        {
            fail(raw_errno());
        }
        for (i, &tfd) in tmp_extra.iter().enumerate().take(ctx.num_extra) {
            if libc::dup2(tfd, 3 + i as i32) < 0 {
                fail(raw_errno());
            }
        }

        let err_target = 3 + ctx.num_extra as i32;
        if libc::dup2(tmp_err, err_target) < 0 {
            fail(raw_errno());
        }
        libc::fcntl(err_target, libc::F_SETFD, libc::FD_CLOEXEC);
        close_fds_from((err_target + 1) as u32);

        if let Some(ref set) = ctx.cpu_set_opt {
            let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set);
        }

        let rlim = libc::rlimit {
            rlim_cur: ctx.stack_limit,
            rlim_max: ctx.stack_limit,
        };
        libc::setrlimit(libc::RLIMIT_STACK, &rlim);

        let rlim_zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &rlim_zero);

        libc::execve(ctx.c_exe_ptr, ctx.c_argv_ptr, ctx.c_env_ptr);
        report_spawn_errno_and_exit(err_target, raw_errno());
    }
}

/// Drains all currently available bytes from a non-blocking pipe fd into `buf` (up to `limit`).
/// Returns `(hit_eof, exceeded_limit)`.
fn drain_pipe_nonblocking(
    fd: i32,
    buf: &mut Vec<u8>,
    limit: usize,
    stop_on_limit: bool,
    chunk: &mut [u8],
) -> (bool, bool) {
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n < 0 {
            let err = unsafe { raw_errno() };
            if err == libc::EINTR {
                continue;
            }
            return (false, false);
        }
        if n == 0 {
            return (true, false);
        }
        let n = n as usize;
        if buf.len() + n > limit {
            let take = limit.saturating_sub(buf.len());
            if take > 0 {
                buf.extend_from_slice(&chunk[..take]);
            }
            if stop_on_limit {
                return (false, true);
            }
            // For stderr: keep draining until EAGAIN/EOF so child never blocks on full pipe.
        } else {
            buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Drains remaining pipe bytes after the child/cgroup has exited, waiting briefly if dying
/// descendants are still releasing pipe write-ends in the kernel.
fn drain_pipe_until_eof(
    fd: i32,
    buf: &mut Vec<u8>,
    limit: usize,
    stop_on_limit: bool,
    chunk: &mut [u8],
) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_millis(50);
    loop {
        let (eof, over) = drain_pipe_nonblocking(fd, buf, limit, stop_on_limit, chunk);
        if over {
            return true;
        }
        if eof {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let rem_ms = (deadline - now).as_millis().clamp(1, 5) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, rem_ms) };
        if r == 0 {
            return false;
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

    // 1. Create dedicated cgroup v2 if cgroup_base is provided.
    // Direct clone3 uses 2 helper processes (PID 1 init + PID 2 contestant); if we fall back to
    // legacy fork+unshare+fork we bump pids.max to LEGACY_HELPER_PROCS before spawning.
    let cgroup = if let Some(base) = cgroup_base {
        let cg = CgroupV2::create_for_run(base, "uj")?;
        let helpers = if used_namespaces {
            DIRECT_HELPER_PROCS
        } else {
            1
        };
        cg.set_limits_with_helpers(&spec.limits, spec.cpu_affinity.as_deref(), helpers)?;
        Some(cg)
    } else {
        None
    };

    // Open per-run memory.peak (O_RDWR on Linux >= 6.12) and cpu.stat handles before fork.
    let mem_peak_fd = cgroup
        .as_ref()
        .and_then(|cg| cg.open_peak_tracking())
        .unwrap_or(-1);
    let child_cpu_stat_file = if used_namespaces {
        cgroup.as_ref().and_then(|cg| cg.open_cpu_stat_for_child())
    } else {
        None
    };
    let child_cpu_stat_fd = child_cpu_stat_file
        .as_ref()
        .map(AsRawFd::as_raw_fd)
        .unwrap_or(-1);
    let cgroup_dir_file = if used_namespaces {
        cgroup.as_ref().and_then(|cg| cg.open_dir_fd())
    } else {
        None
    };

    // 2. Resolve executable & stdio descriptors
    let exe_path = resolve_executable(&spec.argv[0]);
    let host_exe = if exe_path.is_absolute() {
        exe_path
    } else if exe_path.exists() {
        std::env::current_dir()
            .map(|cwd| cwd.join(&exe_path))
            .unwrap_or(exe_path)
    } else {
        let in_work = spec.work_dir.join(&exe_path);
        if in_work.is_absolute() {
            in_work
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&in_work))
                .unwrap_or(in_work)
        }
    };

    // Keep owned File/Pipe handles alive until the child has spawned.
    let mut stdin_file_keep: Option<File> = None;
    let mut stdin_pipe_write_keep: Option<OwnedFd> = None;
    let mut stdin_pipe_read_keep: Option<OwnedFd> = None;
    let mut jail_stdin_host_path: Option<PathBuf> = None;

    let child_stdin_fd: i32 = if let Some(ref fd) = custom_stdin {
        fd.as_raw_fd()
    } else {
        match &spec.stdin {
            Stdio::Null => dev_null_rd()?,
            Stdio::File(path) => {
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    spec.work_dir.join(path)
                };
                if used_namespaces {
                    let c_in = CString::new(resolved.as_os_str().as_bytes())
                        .map_err(|e| SandboxError::Internal(format!("Invalid stdin path: {e}")))?;
                    if unsafe { libc::access(c_in.as_ptr(), libc::R_OK) } != 0 {
                        return Err(SandboxError::Io(std::io::Error::last_os_error()));
                    }
                    jail_stdin_host_path = Some(resolved);
                    dev_null_rd()?
                } else {
                    let f = File::open(&resolved)?;
                    let raw = f.as_raw_fd();
                    stdin_file_keep = Some(f);
                    raw
                }
            }
            Stdio::Pipe => {
                let (r, w) = make_cloexec_pipe()?;
                let raw = r.as_raw_fd();
                stdin_pipe_read_keep = Some(r);
                stdin_pipe_write_keep = Some(w);
                raw
            }
            Stdio::Inherit => 0,
        }
    };

    let capture_stdout = custom_stdout.is_none() && matches!(&spec.stdout, Stdio::Pipe);
    let mut stdout_file_keep: Option<File> = None;
    let mut stdout_read_owned: Option<OwnedFd> = None;
    let mut stdout_write_owned: Option<OwnedFd> = None;

    let child_stdout_fd: i32 = if let Some(ref fd) = custom_stdout {
        fd.as_raw_fd()
    } else {
        match &spec.stdout {
            Stdio::Null => dev_null_wr()?,
            Stdio::File(path) => {
                let f = File::create(path)?;
                let raw = f.as_raw_fd();
                stdout_file_keep = Some(f);
                raw
            }
            Stdio::Pipe => {
                let (r, w) = make_cloexec_pipe()?;
                let raw = w.as_raw_fd();
                stdout_read_owned = Some(r);
                stdout_write_owned = Some(w);
                raw
            }
            Stdio::Inherit => 1,
        }
    };

    let (stderr_read_owned, stderr_write_owned) = make_cloexec_pipe()?;
    let child_stderr_fd = stderr_write_owned.as_raw_fd();

    // 3. Precompute paths, argv, env, rlimits, affinity, and seccomp filter in parent
    let work_dir = &spec.work_dir;
    let stack_limit = spec.limits.stack_bytes.unwrap_or_else(|| {
        spec.limits
            .memory_bytes
            .saturating_sub(4 * 1024 * 1024)
            .max(8 * 1024 * 1024)
    });
    let num_extra = spec.extra_fds.len().min(64);
    let mut extra_fds_arr = [0i32; 64];
    for (dst, src) in extra_fds_arr
        .iter_mut()
        .zip(spec.extra_fds.iter())
        .take(num_extra)
    {
        *dst = src.0 as i32;
    }

    let cgroup_procs_cpath = cgroup
        .as_ref()
        .and_then(|cg| CString::new(cg.path().join("cgroup.procs").as_os_str().as_bytes()).ok());
    let c_workdir = CString::new(work_dir.as_os_str().as_bytes())
        .map_err(|e| SandboxError::Internal(format!("Invalid workdir path: {e}")))?;

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

    let (err_read_owned, err_write_owned) = make_cloexec_pipe()?;
    let err_write_fd = err_write_owned.as_raw_fd();

    // Spawn child process (either untrusted namespace jail or trusted fast vfork+exec)
    let mut status_read_owned_opt: Option<OwnedFd> = None;
    let mut jail_rootfs_guard: Option<tempfile::TempDir> = None;
    let child_pid: i32;
    let pidfd_owned: Option<OwnedFd>;

    if !used_namespaces {
        // Fast trusted path: `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)` avoids copying the
        // supervisor's page tables (critical when JudgePool holds hundreds of MiB of test caches).
        let c_exe = CString::new(host_exe.as_os_str().as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid exe path: {e}")))?;
        let mut c_argv = Vec::with_capacity(spec.argv.len());
        c_argv.push(c_exe.clone());
        for a in spec.argv.iter().skip(1) {
            c_argv.push(CString::new(a.as_bytes()).unwrap_or_default());
        }
        let c_argv_ptrs = SendSyncPtrs::new(c_argv);

        let mut c_env = Vec::with_capacity(spec.env.len());
        for (k, v) in &spec.env {
            let mut kv = Vec::with_capacity(k.len() + 1 + v.len());
            kv.extend_from_slice(k.as_bytes());
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            c_env.push(CString::new(kv).unwrap_or_default());
        }
        let c_env_ptrs = SendSyncPtrs::new(c_env);

        let mut ctx = TrustedVforkCtx {
            stdin_fd: child_stdin_fd,
            stdout_fd: child_stdout_fd,
            stderr_fd: child_stderr_fd,
            extra_fds: extra_fds_arr,
            num_extra,
            err_write_fd,
            cgroup_procs_ptr: cgroup_procs_cpath
                .as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr()),
            mem_peak_fd,
            c_workdir_ptr: c_workdir.as_ptr(),
            cpu_set_opt,
            stack_limit,
            c_exe_ptr: c_exe.as_ptr(),
            c_argv_ptr: c_argv_ptrs.as_ptr(),
            c_env_ptr: c_env_ptrs.as_ptr(),
        };

        let mut vstack = VforkStack([0u8; 32 * 1024]);
        let stack_top = unsafe { vstack.0.as_mut_ptr().add(vstack.0.len()) as *mut libc::c_void };
        let pid = unsafe {
            libc::clone(
                trusted_vfork_trampoline,
                stack_top,
                libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD,
                &mut ctx as *mut TrustedVforkCtx as *mut libc::c_void,
            )
        };
        if pid < 0 {
            return Err(SandboxError::Internal(format!(
                "Failed to spawn child: {}",
                std::io::Error::last_os_error()
            )));
        }
        child_pid = pid;
        pidfd_owned = pidfd_open(child_pid).ok();
    } else {
        let (status_r, status_w) = make_cloexec_pipe()?;
        let status_write_fd = status_w.as_raw_fd();
        status_read_owned_opt = Some(status_r);

        let base = rootfs_base_dir();
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
        let rootfs = td.path().to_path_buf();
        jail_rootfs_guard = Some(td);

        let mk_cstr = |p: &Path| -> Result<CString, SandboxError> {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|e| SandboxError::Internal(format!("Invalid path {}: {e}", p.display())))
        };

        let c_rootfs = mk_cstr(&rootfs)?;
        let c_usr = mk_cstr(&rootfs.join("usr"))?;
        let c_dev = mk_cstr(&rootfs.join("dev"))?;
        let c_proc = mk_cstr(&rootfs.join("proc"))?;
        let c_tmp = mk_cstr(&rootfs.join("tmp"))?;
        let c_box = mk_cstr(&rootfs.join("box"))?;
        let c_etc = mk_cstr(&rootfs.join("etc"))?;
        let c_oldroot = mk_cstr(&rootfs.join("old_root"))?;
        let c_bin = mk_cstr(&rootfs.join("bin"))?;
        let c_lib = mk_cstr(&rootfs.join("lib"))?;
        let c_lib64 = mk_cstr(&rootfs.join("lib64"))?;

        if !host_exe.exists() {
            return Err(SandboxError::Internal(format!(
                "Failed to spawn child: '{}' does not exist",
                host_exe.display()
            )));
        }

        let (jail_exe_path, target_exe_pair) =
            if host_exe.starts_with("/usr") || host_exe.starts_with("/bin") {
                (host_exe.to_string_lossy().into_owned(), None)
            } else {
                let target_exe = rootfs.join("__uj_bin");
                (
                    "/__uj_bin".to_string(),
                    Some((mk_cstr(&host_exe)?, mk_cstr(&target_exe)?)),
                )
            };

        let stdin_bind_pair = if let Some(ref host_in) = jail_stdin_host_path {
            Some((mk_cstr(host_in)?, mk_cstr(&rootfs.join("__uj_stdin"))?))
        } else {
            None
        };

        let mut ro_bind_pairs = Vec::with_capacity(spec.ro_binds.len());
        for ro_bind in &spec.ro_binds {
            if let Ok(meta) = std::fs::metadata(ro_bind) {
                let rel = ro_bind.strip_prefix("/").unwrap_or(ro_bind);
                let target = rootfs.join(rel);
                if let (Ok(c_src), Ok(c_dst)) = (mk_cstr(ro_bind), mk_cstr(&target)) {
                    ro_bind_pairs.push((c_src, c_dst, meta.is_dir()));
                }
            }
        }

        let mut dev_pairs = Vec::with_capacity(4);
        for dev in ["null", "zero", "urandom", "random"] {
            let dst = rootfs.join("dev").join(dev);
            if let (Ok(c_src), Ok(c_dst)) = (CString::new(format!("/dev/{dev}")), mk_cstr(&dst)) {
                dev_pairs.push((c_src, c_dst));
            }
        }

        let ld_so_pair = if host_has_ld_so_cache() {
            let dst_ld = rootfs.join("etc").join("ld.so.cache");
            CString::new("/etc/ld.so.cache")
                .ok()
                .zip(mk_cstr(&dst_ld).ok())
        } else {
            None
        };

        let alt_pair = if host_has_etc_alternatives() {
            let dst_alt = rootfs.join("etc").join("alternatives");
            CString::new("/etc/alternatives")
                .ok()
                .zip(mk_cstr(&dst_alt).ok())
        } else {
            None
        };

        let mut c_argv = Vec::with_capacity(spec.argv.len());
        c_argv.push(
            CString::new(jail_exe_path.as_bytes())
                .map_err(|e| SandboxError::Internal(format!("Invalid jail exe: {e}")))?,
        );
        for a in spec.argv.iter().skip(1) {
            let p = Path::new(a);
            if let Ok(rel) = p.strip_prefix(work_dir) {
                let mapped = if rel.as_os_str().is_empty() {
                    "/box".to_string()
                } else {
                    format!("/box/{}", rel.display())
                };
                c_argv.push(CString::new(mapped.as_bytes()).unwrap_or_default());
            } else {
                c_argv.push(CString::new(a.as_bytes()).unwrap_or_default());
            }
        }
        let c_argv_ptrs = SendSyncPtrs::new(c_argv);

        let mut has_path_env = false;
        let mut c_env = Vec::with_capacity(spec.env.len() + 1);
        for (k, v) in &spec.env {
            if k == OsStr::new("PATH") {
                has_path_env = true;
            }
            let mut kv = Vec::with_capacity(k.len() + 1 + v.len());
            kv.extend_from_slice(k.as_bytes());
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            c_env.push(CString::new(kv).unwrap_or_default());
        }
        if !has_path_env {
            c_env.push(CString::new("PATH=/usr/bin:/bin").unwrap());
        }
        let c_env_ptrs = SendSyncPtrs::new(c_env);
        let c_exe = CString::new(jail_exe_path.as_bytes())
            .map_err(|e| SandboxError::Internal(format!("Invalid jail exe: {e}")))?;

        let real_uid = unsafe { libc::getuid() };
        let real_gid = unsafe { libc::getgid() };
        let inner_uid = if real_uid == 0 { 1000 } else { real_uid };
        let inner_gid = if real_gid == 0 { 1000 } else { real_gid };
        let uid_map_bytes = format!("{inner_uid} {real_uid} 1\n");
        let gid_map_bytes = format!("{inner_gid} {real_gid} 1\n");

        let seccomp_filter = crate::linux::seccomp::prebuilt_filter();
        let mem_limit_bytes = spec.limits.memory_bytes;
        let cpu_limit_ms = spec.limits.cpu_time_ms;
        let max_procs = spec.limits.max_processes;
        let output_limit_bytes = spec.limits.output_bytes;
        let degraded_or_no_cgroup = tier == Tier::Degraded || cgroup.is_none();

        // Try direct `clone3` into all 6 namespaces (+ target cgroup + pidfd) so the child starts
        // immediately as PID 1 without an intermediate `unshare` helper process or extra fork().
        let ns_flags = (libc::CLONE_NEWUSER
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNS
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUTS) as u64;

        let mut out_pidfd: i32 = -1;
        let mut joined_cgroup_in_clone3 = false;
        let mut use_legacy_unshare = false;

        let mut clone_ret: libc::c_long = -1;
        if let Some(ref cg_dir) = cgroup_dir_file {
            let args = CloneArgs {
                flags: ns_flags | CLONE_PIDFD_FLAG | CLONE_INTO_CGROUP_FLAG,
                pidfd: &mut out_pidfd as *mut i32 as u64,
                child_tid: 0,
                parent_tid: 0,
                exit_signal: libc::SIGCHLD as u64,
                stack: 0,
                stack_size: 0,
                tls: 0,
                set_tid: 0,
                set_tid_size: 0,
                cgroup: cg_dir.as_raw_fd() as u64,
            };
            clone_ret = unsafe {
                libc::syscall(
                    SYS_CLONE3_NUM,
                    &args as *const CloneArgs,
                    std::mem::size_of::<CloneArgs>(),
                )
            };
            if clone_ret >= 0 {
                joined_cgroup_in_clone3 = true;
            }
        }

        if clone_ret < 0 {
            out_pidfd = -1;
            let args = CloneArgs {
                flags: ns_flags | CLONE_PIDFD_FLAG,
                pidfd: &mut out_pidfd as *mut i32 as u64,
                child_tid: 0,
                parent_tid: 0,
                exit_signal: libc::SIGCHLD as u64,
                stack: 0,
                stack_size: 0,
                tls: 0,
                set_tid: 0,
                set_tid_size: 0,
                cgroup: 0,
            };
            clone_ret = unsafe {
                libc::syscall(
                    SYS_CLONE3_NUM,
                    &args as *const CloneArgs,
                    std::mem::size_of::<CloneArgs>(),
                )
            };
        }

        if clone_ret < 0 {
            // Fallback to legacy fork() + unshare() + fork() on older/restricted kernels.
            if let Some(ref cg) = cgroup
                && spec.limits.max_processes > 0
            {
                let _ = cg.set_pids_max(
                    (spec.limits.max_processes as u64).saturating_add(LEGACY_HELPER_PROCS),
                );
            }
            use_legacy_unshare = true;
            clone_ret = unsafe { libc::fork() as libc::c_long };
            if clone_ret < 0 {
                return Err(SandboxError::Internal(format!(
                    "Failed to spawn child: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        if clone_ret == 0 {
            // ==================== CHILD PROCESS ====================
            unsafe {
                let fail = |errno: i32| -> ! { report_spawn_errno_and_exit(err_write_fd, errno) };

                if use_legacy_unshare {
                    if let Some(ref procs_cpath) = cgroup_procs_cpath
                        && let Err(e) = raw_write_all(procs_cpath.as_ptr(), b"0\n")
                    {
                        fail(e);
                    }
                    if libc::unshare(
                        libc::CLONE_NEWUSER
                            | libc::CLONE_NEWPID
                            | libc::CLONE_NEWNS
                            | libc::CLONE_NEWNET
                            | libc::CLONE_NEWIPC
                            | libc::CLONE_NEWUTS,
                    ) != 0
                    {
                        fail(raw_errno());
                    }
                }

                // Setup user namespace UID/GID mapping via raw syscalls
                if let Err(e) = raw_write_all(c"/proc/self/setgroups".as_ptr(), b"deny\n") {
                    fail(e);
                }
                if let Err(e) =
                    raw_write_all(c"/proc/self/uid_map".as_ptr(), uid_map_bytes.as_bytes())
                {
                    fail(e);
                }
                if let Err(e) =
                    raw_write_all(c"/proc/self/gid_map".as_ptr(), gid_map_bytes.as_bytes())
                {
                    fail(e);
                }

                // If clone3 succeeded without CLONE_INTO_CGROUP, join cgroup now (host /sys is still
                // visible and our uid_map now maps back to real_uid).
                if !use_legacy_unshare
                    && !joined_cgroup_in_clone3
                    && let Some(ref procs_cpath) = cgroup_procs_cpath
                    && let Err(e) = raw_write_all(procs_cpath.as_ptr(), b"0\n")
                {
                    fail(e);
                }

                // Mark non-dumpable after writing uid_map/gid_map so /proc/1/* are root-owned
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

                // Make all mounts private so rootfs mounts don't propagate to host
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    fail(raw_errno());
                }

                // Mount private tmpfs rootfs
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_rootfs.as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    c"size=64m".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    fail(raw_errno());
                }

                // Create directory skeleton on fresh tmpfs
                for ptr in [
                    c_usr.as_ptr(),
                    c_dev.as_ptr(),
                    c_proc.as_ptr(),
                    c_tmp.as_ptr(),
                    c_box.as_ptr(),
                    c_etc.as_ptr(),
                    c_oldroot.as_ptr(),
                ] {
                    if libc::mkdir(ptr, 0o755) != 0 && raw_errno() != libc::EEXIST {
                        fail(raw_errno());
                    }
                }

                let _ = libc::symlink(c"usr/bin".as_ptr(), c_bin.as_ptr());
                let _ = libc::symlink(c"usr/lib".as_ptr(), c_lib.as_ptr());
                let _ = libc::symlink(c"usr/lib64".as_ptr(), c_lib64.as_ptr());

                // Bind mount /usr recursively read-only (including all submounts)
                if let Err(e) = bind_mount_ro(
                    c"/usr".as_ptr(),
                    c_usr.as_ptr(),
                    true,
                    MOUNT_ATTR_NOSUID_FLAG,
                    libc::MS_REC | libc::MS_NOSUID,
                ) {
                    fail(e);
                }

                // Bind mount /box (work_dir) read-write
                if libc::mount(
                    c_workdir.as_ptr(),
                    c_box.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ) != 0
                {
                    fail(raw_errno());
                }

                // Bind mount binary if outside /usr
                if let Some((ref c_host_exe, ref c_target_exe)) = target_exe_pair {
                    let fd = libc::open(
                        c_target_exe.as_ptr(),
                        libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                        0o755,
                    );
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    if let Err(e) = bind_mount_ro(
                        c_host_exe.as_ptr(),
                        c_target_exe.as_ptr(),
                        false,
                        MOUNT_ATTR_NOSUID_FLAG,
                        libc::MS_NOSUID,
                    ) {
                        fail(e);
                    }
                }

                // Bind mount stdin input file read-only so /proc/self/fd/0 is backed by an MS_RDONLY mount
                if let Some((ref c_host_in, ref c_target_in)) = stdin_bind_pair {
                    let fd = libc::open(
                        c_target_in.as_ptr(),
                        libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                        0o444,
                    );
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    if let Err(e) = bind_mount_ro(
                        c_host_in.as_ptr(),
                        c_target_in.as_ptr(),
                        false,
                        MOUNT_ATTR_NOSUID_FLAG | MOUNT_ATTR_NODEV_FLAG,
                        libc::MS_NOSUID | libc::MS_NODEV,
                    ) {
                        fail(e);
                    }
                }

                // Additional read-only binds
                for (c_src, c_dst, is_dir) in &ro_bind_pairs {
                    if *is_dir {
                        let _ = libc::mkdir(c_dst.as_ptr(), 0o755);
                    } else {
                        let fd = libc::open(
                            c_dst.as_ptr(),
                            libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                            0o644,
                        );
                        if fd >= 0 {
                            libc::close(fd);
                        }
                    }
                    if let Err(e) = bind_mount_ro(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        true,
                        MOUNT_ATTR_NOSUID_FLAG,
                        libc::MS_REC | libc::MS_NOSUID,
                    ) {
                        fail(e);
                    }
                }

                // Private /tmp
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_tmp.as_ptr(),
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    c"size=16m,mode=1777".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    fail(raw_errno());
                }

                // Private /dev (character devices /dev/null, /dev/zero, /dev/urandom, /dev/random
                // must remain writable so `> /dev/null` works, while keeping MS_NOSUID | MS_NOEXEC).
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    c_dev.as_ptr(),
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NOEXEC,
                    c"size=4m,mode=0755".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    fail(raw_errno());
                }

                for (c_src, c_dst) in &dev_pairs {
                    let fd = libc::open(
                        c_dst.as_ptr(),
                        libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                        0o666,
                    );
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
                }
                let _ = libc::mount(
                    std::ptr::null(),
                    c_dev.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC,
                    std::ptr::null(),
                );

                if let Some((ref c_src, ref c_dst)) = ld_so_pair {
                    let fd = libc::open(
                        c_dst.as_ptr(),
                        libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                        0o644,
                    );
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    let _ = bind_mount_ro(c_src.as_ptr(), c_dst.as_ptr(), false, 0, 0);
                }

                if let Some((ref c_src, ref c_dst)) = alt_pair {
                    let _ = libc::mkdir(c_dst.as_ptr(), 0o755);
                    let _ = bind_mount_ro(
                        c_src.as_ptr(),
                        c_dst.as_ptr(),
                        true,
                        MOUNT_ATTR_NOSUID_FLAG,
                        libc::MS_REC | libc::MS_NOSUID,
                    );
                }

                // pivot_root into isolated rootfs
                if libc::syscall(libc::SYS_pivot_root, c_rootfs.as_ptr(), c_oldroot.as_ptr()) != 0 {
                    fail(raw_errno());
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    fail(raw_errno());
                }
                if libc::umount2(c"/old_root".as_ptr(), libc::MNT_DETACH) != 0 {
                    fail(raw_errno());
                }
                let _ = libc::rmdir(c"/old_root".as_ptr());

                // Ifstdin comes from a host file, open it from the read-only bind inside the jail,
                // then detach and unlink the mountpoint.
                let effective_stdin_fd = if stdin_bind_pair.is_some() {
                    let in_fd = libc::open(c"/__uj_stdin".as_ptr(), libc::O_RDONLY);
                    if in_fd < 0 {
                        fail(raw_errno());
                    }
                    let _ = libc::umount2(c"/__uj_stdin".as_ptr(), libc::MNT_DETACH);
                    let _ = libc::unlink(c"/__uj_stdin".as_ptr());
                    in_fd
                } else {
                    child_stdin_fd
                };

                // Remount rootfs read-only so only /box and /tmp are writable
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                ) != 0
                {
                    fail(raw_errno());
                }

                if libc::chdir(c"/box".as_ptr()) != 0 {
                    fail(raw_errno());
                }

                // If we fell back to legacy unshare(CLONE_NEWPID), fork once to enter PID 1.
                if use_legacy_unshare {
                    let init_pid = libc::fork();
                    if init_pid < 0 {
                        fail(raw_errno());
                    }
                    if init_pid > 0 {
                        close_fds_from(0);
                        let mut st = 0;
                        while libc::waitpid(init_pid, &mut st, 0) < 0 {
                            if raw_errno() != libc::EINTR {
                                break;
                            }
                        }
                        if libc::WIFEXITED(st) {
                            libc::_exit(libc::WEXITSTATUS(st));
                        } else if libc::WIFSIGNALED(st) {
                            libc::_exit(128 + libc::WTERMSIG(st));
                        } else {
                            libc::_exit(1);
                        }
                    }
                    libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                }

                // We are now PID 1 inside the isolated PID namespace!
                // Collision-free fd shuffle: move all needed fds above min_safe, then place
                // 0, 1, 2, 3..3+num_extra, status_tx_fd, peak_fd, cpu_stat_fd, and close everything else.
                let min_safe = 3 + (num_extra as i32) + 16;
                let dup_safe = |fd: i32| -> Result<i32, i32> {
                    let d = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min_safe);
                    if d < 0 { Err(raw_errno()) } else { Ok(d) }
                };

                let tmp_in = match dup_safe(effective_stdin_fd) {
                    Ok(fd) => fd,
                    Err(e) => fail(e),
                };
                let tmp_out = match dup_safe(child_stdout_fd) {
                    Ok(fd) => fd,
                    Err(e) => fail(e),
                };
                let tmp_stderr = match dup_safe(child_stderr_fd) {
                    Ok(fd) => fd,
                    Err(e) => fail(e),
                };
                let mut tmp_extra = [0i32; 64];
                for (slot, &src_fd) in tmp_extra
                    .iter_mut()
                    .zip(extra_fds_arr.iter())
                    .take(num_extra)
                {
                    *slot = match dup_safe(src_fd) {
                        Ok(fd) => fd,
                        Err(e) => fail(e),
                    };
                }
                let tmp_status = match dup_safe(status_write_fd) {
                    Ok(fd) => fd,
                    Err(e) => fail(e),
                };
                let tmp_peak = if mem_peak_fd >= 0 {
                    dup_safe(mem_peak_fd).unwrap_or(-1)
                } else {
                    -1
                };
                let tmp_cpu_stat = if child_cpu_stat_fd >= 0 {
                    dup_safe(child_cpu_stat_fd).unwrap_or(-1)
                } else {
                    -1
                };

                if libc::dup2(tmp_in, 0) < 0
                    || libc::dup2(tmp_out, 1) < 0
                    || libc::dup2(tmp_stderr, 2) < 0
                {
                    fail(raw_errno());
                }
                for (i, &tfd) in tmp_extra.iter().enumerate().take(num_extra) {
                    if libc::dup2(tfd, 3 + i as i32) < 0 {
                        fail(raw_errno());
                    }
                }

                let mut next_internal = 3 + num_extra as i32;
                let status_tx_fd = next_internal;
                next_internal += 1;
                if libc::dup2(tmp_status, status_tx_fd) < 0 {
                    fail(raw_errno());
                }
                libc::fcntl(status_tx_fd, libc::F_SETFD, libc::FD_CLOEXEC);

                let peak_fd_target = if tmp_peak >= 0 {
                    let t = next_internal;
                    if libc::dup2(tmp_peak, t) >= 0 {
                        libc::fcntl(t, libc::F_SETFD, libc::FD_CLOEXEC);
                        next_internal += 1;
                        t
                    } else {
                        -1
                    }
                } else {
                    -1
                };

                let cpu_stat_target = if tmp_cpu_stat >= 0 {
                    let t = next_internal;
                    if libc::dup2(tmp_cpu_stat, t) >= 0 {
                        libc::fcntl(t, libc::F_SETFD, libc::FD_CLOEXEC);
                        next_internal += 1;
                        t
                    } else {
                        -1
                    }
                } else {
                    -1
                };

                // Mount isolated /proc for the new PID namespace
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

                // Close all fds >= next_internal (including err_write_fd, signalling setup EOF to supervisor!)
                close_fds_from(next_internal as u32);

                // Reset per-open-file memory.peak watermark (Linux >= 6.12) right before spawning PID 2
                if peak_fd_target >= 0 {
                    let _ = libc::write(peak_fd_target, b"0\n".as_ptr() as *const libc::c_void, 2);
                    libc::close(peak_fd_target);
                }

                // Sample cgroup CPU usage and monotonic clock right before spawning contestant (PID 2)
                let cpu_before = read_cpu_stat_fd_us(cpu_stat_target);
                let wall_before_ns = monotonic_ns();

                let mut vctx = ContestantVforkCtx {
                    status_tx_fd,
                    cpu_stat_fd: cpu_stat_target,
                    cpu_set_opt,
                    stack_limit,
                    output_limit_bytes,
                    degraded_or_no_cgroup,
                    mem_limit_bytes,
                    cpu_limit_ms,
                    max_procs,
                    seccomp_filter,
                    c_exe_ptr: c_exe.as_ptr(),
                    c_argv_ptr: c_argv_ptrs.as_ptr(),
                    c_env_ptr: c_env_ptrs.as_ptr(),
                };

                // Spawn PID 2 via `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)` to avoid copying page tables
                let mut vstack = VforkStack([0u8; 32 * 1024]);
                let stack_top = vstack.0.as_mut_ptr().add(vstack.0.len()) as *mut libc::c_void;
                let mut contestant_pid = libc::clone(
                    contestant_vfork_trampoline,
                    stack_top,
                    libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD,
                    &mut vctx as *mut ContestantVforkCtx as *mut libc::c_void,
                );
                if contestant_pid < 0 {
                    // Fallback to fork() if vfork clone failed
                    contestant_pid = libc::fork();
                    if contestant_pid < 0 {
                        libc::_exit(126);
                    }
                    if contestant_pid == 0 {
                        run_contestant_exec(&vctx);
                    }
                }

                // PID 1 init: wait for contestant PID 2, then reap any orphan zombies
                let mut status = 0;
                let mut rusage: libc::rusage = std::mem::zeroed();
                while libc::wait4(contestant_pid, &mut status, 0, &mut rusage) < 0 {
                    if raw_errno() != libc::EINTR {
                        break;
                    }
                }
                while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}

                let wall_after_ns = monotonic_ns();
                let cpu_after = read_cpu_stat_fd_us(cpu_stat_target);
                if cpu_stat_target >= 0 {
                    libc::close(cpu_stat_target);
                }

                let (has_cgroup_cpu, contestant_cpu_us) = match (cpu_before, cpu_after) {
                    (Some(b), Some(a)) => (1u8, a.saturating_sub(b)),
                    _ => (0u8, 0u64),
                };
                let contestant_wall_us = if wall_before_ns > 0 && wall_after_ns >= wall_before_ns {
                    (wall_after_ns - wall_before_ns) / 1000
                } else {
                    0
                };

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
                    has_cgroup_cpu,
                    pad: [0; 2],
                    code_or_sig,
                    ru_utime_us: (rusage.ru_utime.tv_sec as u64) * 1_000_000
                        + (rusage.ru_utime.tv_usec as u64),
                    ru_stime_us: (rusage.ru_stime.tv_sec as u64) * 1_000_000
                        + (rusage.ru_stime.tv_usec as u64),
                    ru_maxrss_kb: rusage.ru_maxrss as u64,
                    contestant_cpu_us,
                    contestant_wall_us,
                };

                let packet_bytes = std::slice::from_raw_parts(
                    &packet as *const SandboxStatusPacket as *const u8,
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
        }

        child_pid = clone_ret as i32;
        if out_pidfd >= 0 {
            pidfd_owned = Some(unsafe { OwnedFd::from_raw_fd(out_pidfd) });
        } else {
            pidfd_owned = pidfd_open(child_pid).ok();
        }
        drop(status_w);
    }

    // ==================== SUPERVISOR PROCESS ====================
    // Drop child-side handles immediately so pipes observe true EOF when child exits.
    drop(err_write_owned);
    drop(stdout_write_owned);
    drop(stderr_write_owned);
    drop(stdin_file_keep);
    drop(stdin_pipe_read_keep);
    drop(stdout_file_keep);
    drop(child_cpu_stat_file);
    drop(cgroup_dir_file);
    drop(custom_stdin);
    drop(custom_stdout);
    drop(extra_owned_fds);

    // Wait for child setup completion (or early spawn failure errno) via err_pipe.
    let mut err_buf = [0u8; 4];
    let mut err_read = 0usize;
    while err_read < 4 {
        let n = unsafe {
            libc::read(
                err_read_owned.as_raw_fd(),
                err_buf[err_read..].as_mut_ptr() as *mut libc::c_void,
                4 - err_read,
            )
        };
        if n < 0 {
            if unsafe { raw_errno() } == libc::EINTR {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        err_read += n as usize;
    }
    drop(err_read_owned);

    if err_read == 4 {
        let errno = i32::from_ne_bytes(err_buf);
        drop(pidfd_owned);
        if let Some(ref cg) = cgroup {
            cg.kill();
        }
        let _ = wait4_pid(child_pid, 0);
        cleanup_rootfs_dir(jail_rootfs_guard);
        return Err(SandboxError::Internal(format!(
            "Failed to spawn child: {}",
            std::io::Error::from_raw_os_error(errno)
        )));
    }

    // Child setup succeeded and contestant execution is starting now!
    let start_time = Instant::now();

    let stdout_limit = spec.limits.output_bytes as usize;
    let stderr_limit = spec.limits.stderr_bytes as usize;
    let mut stdout_buf = if capture_stdout {
        Vec::with_capacity(stdout_limit.min(65536))
    } else {
        Vec::new()
    };
    let mut stderr_buf = Vec::with_capacity(stderr_limit.min(16384));
    let mut stdout_truncated = false;

    if let Some(ref out_fd) = stdout_read_owned {
        set_nonblocking(out_fd.as_raw_fd());
    }
    set_nonblocking(stderr_read_owned.as_raw_fd());

    let mut stdout_open = stdout_read_owned.is_some();
    let mut stderr_open = true;
    let mut chunk = [0u8; 65536];

    let mut killed_by: Option<KillReason> = None;
    let mut waited_result: Option<(std::process::ExitStatus, libc::rusage)> = None;

    // Give a small 5 ms live-kill cushion in namespace mode so ~1 ms of PID 1 setup inside the
    // cgroup never triggers a premature cgroup.kill() right at the contestant's exact CPU boundary
    // while still ensuring genuine CPU loops are stopped promptly with >= cpu_time_ms CPU used.
    let live_cpu_kill_us = if used_namespaces {
        spec.limits
            .cpu_time_ms
            .saturating_mul(1000)
            .saturating_add(5_000)
    } else {
        spec.limits
            .cpu_time_ms
            .saturating_mul(1000)
            .saturating_add(1_000)
    };

    // Single-threaded multiplexed `poll()` loop over pidfd + non-blocking stdout + non-blocking stderr.
    loop {
        if cancel.is_cancelled() {
            killed_by = Some(KillReason::UserCancel);
            if let Some(ref cg) = cgroup {
                cg.kill();
            }
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
            }
            break;
        }

        let wall_ms = start_time.elapsed().as_millis() as u64;
        if wall_ms > spec.limits.wall_time_ms {
            killed_by = Some(KillReason::WallLimit);
            if let Some(ref cg) = cgroup {
                cg.kill();
            }
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
            }
            break;
        }

        if let Some(ref cg) = cgroup {
            if spec.limits.cpu_time_ms > 0 && cg.read_cpu_time_us() > live_cpu_kill_us {
                killed_by = Some(KillReason::CpuLimit);
                cg.kill();
                unsafe {
                    libc::kill(child_pid, libc::SIGKILL);
                }
                break;
            }

            if cg.read_oom_killed() {
                killed_by = Some(KillReason::MemoryLimit);
                cg.kill();
                unsafe {
                    libc::kill(child_pid, libc::SIGKILL);
                }
                break;
            }
        }

        if pidfd_owned.is_none() {
            match wait4_pid(child_pid, libc::WNOHANG) {
                Ok(Some(res)) => {
                    waited_result = Some(res);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }

        let mut pollfds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; 3];
        let mut nfds = 0usize;
        let mut pidfd_slot: Option<usize> = None;
        let mut stdout_slot: Option<usize> = None;
        let mut stderr_slot: Option<usize> = None;

        if let Some(ref pfd) = pidfd_owned {
            pidfd_slot = Some(nfds);
            pollfds[nfds] = libc::pollfd {
                fd: pfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }
        if stdout_open && let Some(ref out_fd) = stdout_read_owned {
            stdout_slot = Some(nfds);
            pollfds[nfds] = libc::pollfd {
                fd: out_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }
        if stderr_open {
            stderr_slot = Some(nfds);
            pollfds[nfds] = libc::pollfd {
                fd: stderr_read_owned.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }

        let rem_wall = spec
            .limits
            .wall_time_ms
            .saturating_sub(wall_ms)
            .saturating_add(1);
        let timeout_ms = rem_wall.clamp(1, 5) as libc::c_int;

        let poll_rc = if nfds > 0 {
            unsafe { libc::poll(pollfds.as_mut_ptr(), nfds as libc::nfds_t, timeout_ms) }
        } else {
            thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            0
        };

        if poll_rc > 0 {
            if let Some(idx) = stdout_slot
                && (pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
                && let Some(ref out_fd) = stdout_read_owned
            {
                let (eof, over) = drain_pipe_nonblocking(
                    out_fd.as_raw_fd(),
                    &mut stdout_buf,
                    stdout_limit,
                    true,
                    &mut chunk,
                );
                if over {
                    stdout_truncated = true;
                    stdout_open = false;
                    killed_by = Some(KillReason::OutputLimit);
                    if let Some(ref cg) = cgroup {
                        cg.kill();
                    }
                    unsafe {
                        libc::kill(child_pid, libc::SIGKILL);
                    }
                    break;
                }
                if eof {
                    stdout_open = false;
                }
            }

            if let Some(idx) = stderr_slot
                && (pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
            {
                let (eof, _) = drain_pipe_nonblocking(
                    stderr_read_owned.as_raw_fd(),
                    &mut stderr_buf,
                    stderr_limit,
                    false,
                    &mut chunk,
                );
                if eof {
                    stderr_open = false;
                }
            }

            if let Some(idx) = pidfd_slot
                && (pollfds[idx].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
            {
                break;
            }
        }
    }

    drop(pidfd_owned);

    let (status, wait_rusage) = match waited_result {
        Some(res) => res,
        None => match wait4_pid(child_pid, 0)? {
            Some(res) => res,
            None => (std::process::ExitStatus::from_raw(0), unsafe {
                std::mem::zeroed()
            }),
        },
    };
    let supervisor_wall_us = start_time.elapsed().as_micros() as u64;

    // Immediately kill any surviving orphan processes in this cgroup so all pipe write-ends close.
    // `cg.kill()` is idempotent and skips redundant writes once `cgroup.kill` has succeeded.
    if let Some(ref cg) = cgroup {
        cg.kill();
    }

    // Drain any remaining bytes buffered in stdout/stderr pipes after process exit.
    if stdout_open
        && let Some(ref out_fd) = stdout_read_owned
        && drain_pipe_until_eof(
            out_fd.as_raw_fd(),
            &mut stdout_buf,
            stdout_limit,
            true,
            &mut chunk,
        )
    {
        stdout_truncated = true;
    }
    if stderr_open {
        let _ = drain_pipe_until_eof(
            stderr_read_owned.as_raw_fd(),
            &mut stderr_buf,
            stderr_limit,
            false,
            &mut chunk,
        );
    }

    drop(stdout_read_owned);
    drop(stderr_read_owned);
    drop(stdin_pipe_write_keep);

    // Read status packet from PID 1 pipe: accept ONLY if exactly one valid packet followed by EOF.
    let status_from_packet = if let Some(status_read_owned) = status_read_owned_opt {
        unsafe {
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
                    if raw_errno() == libc::EINTR {
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
                    &mut packet as *mut SandboxStatusPacket as *mut u8,
                    PKT_SIZE,
                );
                if packet.is_signaled <= 1 && packet.has_cgroup_cpu <= 1 && packet.pad == [0; 2] {
                    Some(packet)
                } else {
                    None
                }
            } else {
                None
            }
        }
    } else {
        None
    };

    let (cpu_time_us, memory_peak_bytes, oom_killed) = if let Some(ref cg) = cgroup {
        let cpu_us = if let Some(ref pkt) = status_from_packet {
            let rusage_cpu = pkt.ru_utime_us.saturating_add(pkt.ru_stime_us);
            if pkt.has_cgroup_cpu != 0 {
                pkt.contestant_cpu_us.max(rusage_cpu)
            } else {
                rusage_cpu
            }
        } else {
            cg.read_cpu_time_us()
        };
        (cpu_us, cg.read_peak_memory(), cg.read_oom_killed())
    } else if let Some(ref pkt) = status_from_packet {
        (
            pkt.ru_utime_us.saturating_add(pkt.ru_stime_us),
            pkt.ru_maxrss_kb.saturating_mul(1024),
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

    let wall_time_us = match status_from_packet {
        Some(ref pkt) if pkt.contestant_wall_us > 0 => pkt.contestant_wall_us,
        _ => supervisor_wall_us,
    };

    let captured_stdout = if capture_stdout {
        Some(CapturedOutput {
            bytes: stdout_buf,
            truncated: stdout_truncated,
        })
    } else {
        None
    };

    cleanup_rootfs_dir(jail_rootfs_guard);

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
    } else if stdout_truncated && killed_by.is_none() {
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
        stderr_tail: stderr_buf,
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
        let (read_fd, write_fd) = make_cloexec_pipe()?;
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
            let (p_to_m_read, p_to_m_write) = make_cloexec_pipe()?;
            let (m_to_p_read, m_to_p_write) = make_cloexec_pipe()?;

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
                .push(OsString::from(r_target.to_string()));
            specs[mgr_idx]
                .argv
                .push(OsString::from(w_target.to_string()));
            star_fds_env.push(format!("{r_target},{w_target}"));
        }
        specs[mgr_idx].env.push((
            OsString::from("UJ_STAR_FDS"),
            OsString::from(star_fds_env.join(";")),
        ));
    }

    // Run specs concurrently in worker threads
    thread::scope(|s| {
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
    })
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

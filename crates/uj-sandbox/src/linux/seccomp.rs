//! Minimal BPF seccomp filter for Linux sandbox (x86_64 and aarch64).

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct sock_filter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

#[repr(C)]
struct sock_fprog {
    len: u16,
    filter: *const sock_filter,
}

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
const SECCOMP_RET_ERRNO: u32 = 0x00050000;

#[allow(dead_code)]
pub(crate) const AUDIT_ARCH_X86_64: u32 = 0xc000003e;
#[allow(dead_code)]
pub(crate) const AUDIT_ARCH_AARCH64: u32 = 0xc00000b7;

// System call numbers on x86_64
#[allow(dead_code)]
pub(crate) mod x86_64_nr {
    pub const SYS_CLONE: u32 = 56;
    pub const SYS_PTRACE: u32 = 101;
    pub const SYS_PIVOT_ROOT: u32 = 155;
    pub const SYS_ADJTIMEX: u32 = 159;
    pub const SYS_CHROOT: u32 = 161;
    pub const SYS_ACCT: u32 = 163;
    pub const SYS_SETTIMEOFDAY: u32 = 164;
    pub const SYS_MOUNT: u32 = 165;
    pub const SYS_UMOUNT2: u32 = 166;
    pub const SYS_SWAPON: u32 = 167;
    pub const SYS_SWAPOFF: u32 = 168;
    pub const SYS_REBOOT: u32 = 169;
    pub const SYS_INIT_MODULE: u32 = 175;
    pub const SYS_DELETE_MODULE: u32 = 176;
    pub const SYS_QUOTACTL: u32 = 179;
    pub const SYS_CLOCK_SETTIME: u32 = 227;
    pub const SYS_KEXEC_LOAD: u32 = 246;
    pub const SYS_ADD_KEY: u32 = 248;
    pub const SYS_REQUEST_KEY: u32 = 249;
    pub const SYS_KEYCTL: u32 = 250;
    pub const SYS_UNSHARE: u32 = 272;
    pub const SYS_PERF_EVENT_OPEN: u32 = 298;
    pub const SYS_FANOTIFY_INIT: u32 = 300;
    pub const SYS_NAME_TO_HANDLE_AT: u32 = 303;
    pub const SYS_OPEN_BY_HANDLE_AT: u32 = 304;
    pub const SYS_CLOCK_ADJTIME: u32 = 305;
    pub const SYS_SETNS: u32 = 308;
    pub const SYS_PROCESS_VM_READV: u32 = 310;
    pub const SYS_PROCESS_VM_WRITEV: u32 = 311;
    pub const SYS_KCMP: u32 = 312;
    pub const SYS_FINIT_MODULE: u32 = 313;
    pub const SYS_KEXEC_FILE_LOAD: u32 = 320;
    pub const SYS_BPF: u32 = 321;
    pub const SYS_USERFAULTFD: u32 = 323;
    pub const SYS_IO_URING_SETUP: u32 = 425;
    pub const SYS_IO_URING_ENTER: u32 = 426;
    pub const SYS_IO_URING_REGISTER: u32 = 427;
    pub const SYS_OPEN_TREE: u32 = 428;
    pub const SYS_MOVE_MOUNT: u32 = 429;
    pub const SYS_FSOPEN: u32 = 430;
    pub const SYS_FSCONFIG: u32 = 431;
    pub const SYS_FSMOUNT: u32 = 432;
    pub const SYS_FSPICK: u32 = 433;
    pub const SYS_CLONE3: u32 = 435;
    pub const SYS_PIDFD_GETFD: u32 = 438;
    pub const SYS_MOUNT_SETATTR: u32 = 442;
    pub const SYS_QUOTACTL_FD: u32 = 443;

    pub const BLACKLIST: [u32; 45] = [
        SYS_PTRACE,
        SYS_PIVOT_ROOT,
        SYS_ADJTIMEX,
        SYS_CHROOT,
        SYS_ACCT,
        SYS_SETTIMEOFDAY,
        SYS_MOUNT,
        SYS_UMOUNT2,
        SYS_SWAPON,
        SYS_SWAPOFF,
        SYS_REBOOT,
        SYS_INIT_MODULE,
        SYS_DELETE_MODULE,
        SYS_QUOTACTL,
        SYS_CLOCK_SETTIME,
        SYS_KEXEC_LOAD,
        SYS_ADD_KEY,
        SYS_REQUEST_KEY,
        SYS_KEYCTL,
        SYS_UNSHARE,
        SYS_PERF_EVENT_OPEN,
        SYS_FANOTIFY_INIT,
        SYS_NAME_TO_HANDLE_AT,
        SYS_OPEN_BY_HANDLE_AT,
        SYS_CLOCK_ADJTIME,
        SYS_SETNS,
        SYS_PROCESS_VM_READV,
        SYS_PROCESS_VM_WRITEV,
        SYS_KCMP,
        SYS_FINIT_MODULE,
        SYS_KEXEC_FILE_LOAD,
        SYS_BPF,
        SYS_USERFAULTFD,
        SYS_IO_URING_SETUP,
        SYS_IO_URING_ENTER,
        SYS_IO_URING_REGISTER,
        SYS_OPEN_TREE,
        SYS_MOVE_MOUNT,
        SYS_FSOPEN,
        SYS_FSCONFIG,
        SYS_FSMOUNT,
        SYS_FSPICK,
        SYS_PIDFD_GETFD,
        SYS_MOUNT_SETATTR,
        SYS_QUOTACTL_FD,
    ];
}

// System call numbers on aarch64 (asm-generic/unistd.h)
#[allow(dead_code)]
pub(crate) mod aarch64_nr {
    pub const SYS_UMOUNT2: u32 = 39;
    pub const SYS_MOUNT: u32 = 40;
    pub const SYS_PIVOT_ROOT: u32 = 41;
    pub const SYS_CHROOT: u32 = 51;
    pub const SYS_QUOTACTL: u32 = 60;
    pub const SYS_ACCT: u32 = 89;
    pub const SYS_UNSHARE: u32 = 97;
    pub const SYS_KEXEC_LOAD: u32 = 104;
    pub const SYS_INIT_MODULE: u32 = 105;
    pub const SYS_DELETE_MODULE: u32 = 106;
    pub const SYS_CLOCK_SETTIME: u32 = 112;
    pub const SYS_PTRACE: u32 = 117;
    pub const SYS_REBOOT: u32 = 142;
    pub const SYS_SETTIMEOFDAY: u32 = 170;
    pub const SYS_ADJTIMEX: u32 = 171;
    pub const SYS_ADD_KEY: u32 = 217;
    pub const SYS_REQUEST_KEY: u32 = 218;
    pub const SYS_KEYCTL: u32 = 219;
    pub const SYS_CLONE: u32 = 220;
    pub const SYS_SWAPON: u32 = 224;
    pub const SYS_SWAPOFF: u32 = 225;
    pub const SYS_PERF_EVENT_OPEN: u32 = 241;
    pub const SYS_FANOTIFY_INIT: u32 = 262;
    pub const SYS_NAME_TO_HANDLE_AT: u32 = 264;
    pub const SYS_OPEN_BY_HANDLE_AT: u32 = 265;
    pub const SYS_CLOCK_ADJTIME: u32 = 266;
    pub const SYS_SETNS: u32 = 268;
    pub const SYS_PROCESS_VM_READV: u32 = 270;
    pub const SYS_PROCESS_VM_WRITEV: u32 = 271;
    pub const SYS_KCMP: u32 = 272;
    pub const SYS_FINIT_MODULE: u32 = 273;
    pub const SYS_BPF: u32 = 280;
    pub const SYS_USERFAULTFD: u32 = 282;
    pub const SYS_KEXEC_FILE_LOAD: u32 = 294;
    pub const SYS_IO_URING_SETUP: u32 = 425;
    pub const SYS_IO_URING_ENTER: u32 = 426;
    pub const SYS_IO_URING_REGISTER: u32 = 427;
    pub const SYS_OPEN_TREE: u32 = 428;
    pub const SYS_MOVE_MOUNT: u32 = 429;
    pub const SYS_FSOPEN: u32 = 430;
    pub const SYS_FSCONFIG: u32 = 431;
    pub const SYS_FSMOUNT: u32 = 432;
    pub const SYS_FSPICK: u32 = 433;
    pub const SYS_CLONE3: u32 = 435;
    pub const SYS_PIDFD_GETFD: u32 = 438;
    pub const SYS_MOUNT_SETATTR: u32 = 442;
    pub const SYS_QUOTACTL_FD: u32 = 443;

    pub const BLACKLIST: [u32; 45] = [
        SYS_PTRACE,
        SYS_PIVOT_ROOT,
        SYS_ADJTIMEX,
        SYS_CHROOT,
        SYS_ACCT,
        SYS_SETTIMEOFDAY,
        SYS_MOUNT,
        SYS_UMOUNT2,
        SYS_SWAPON,
        SYS_SWAPOFF,
        SYS_REBOOT,
        SYS_INIT_MODULE,
        SYS_DELETE_MODULE,
        SYS_QUOTACTL,
        SYS_CLOCK_SETTIME,
        SYS_KEXEC_LOAD,
        SYS_ADD_KEY,
        SYS_REQUEST_KEY,
        SYS_KEYCTL,
        SYS_UNSHARE,
        SYS_PERF_EVENT_OPEN,
        SYS_FANOTIFY_INIT,
        SYS_NAME_TO_HANDLE_AT,
        SYS_OPEN_BY_HANDLE_AT,
        SYS_CLOCK_ADJTIME,
        SYS_SETNS,
        SYS_PROCESS_VM_READV,
        SYS_PROCESS_VM_WRITEV,
        SYS_KCMP,
        SYS_FINIT_MODULE,
        SYS_KEXEC_FILE_LOAD,
        SYS_BPF,
        SYS_USERFAULTFD,
        SYS_IO_URING_SETUP,
        SYS_IO_URING_ENTER,
        SYS_IO_URING_REGISTER,
        SYS_OPEN_TREE,
        SYS_MOVE_MOUNT,
        SYS_FSOPEN,
        SYS_FSCONFIG,
        SYS_FSMOUNT,
        SYS_FSPICK,
        SYS_PIDFD_GETFD,
        SYS_MOUNT_SETATTR,
        SYS_QUOTACTL_FD,
    ];
}

// Namespace flags for clone:
// CLONE_NEWNS (0x00020000) | CLONE_NEWCGROUP (0x02000000) | CLONE_NEWUTS (0x04000000)
// | CLONE_NEWIPC (0x08000000) | CLONE_NEWUSER (0x10000000) | CLONE_NEWPID (0x20000000)
// | CLONE_NEWNET (0x40000000) = 0x7e020000
pub(crate) const CLONE_NAMESPACE_FLAGS: u32 = 0x7e020000;

#[derive(Clone, Copy)]
enum JumpTarget {
    Allow,
    Eperm,
    Enosys,
    Relative(u8),
}

struct Inst {
    code: u16,
    jt: JumpTarget,
    jf: JumpTarget,
    k: u32,
}

pub(crate) fn build_bpf_filter(
    audit_arch: u32,
    block_x32: bool,
    sys_clone3: u32,
    sys_clone: u32,
    blacklist: &[u32],
) -> Vec<sock_filter> {
    let mut insts = vec![
        // [0] Load arch (offset 4)
        Inst {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: 4,
        },
        // [1] Check arch == audit_arch
        Inst {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: JumpTarget::Relative(1), // skip kill instruction
            jf: JumpTarget::Relative(0),
            k: audit_arch,
        },
        // [2] Kill if architecture mismatch
        Inst {
            code: BPF_RET | BPF_K,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: SECCOMP_RET_KILL_PROCESS,
        },
        // [3] Load syscall number (offset 0)
        Inst {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: 0,
        },
    ];

    if block_x32 {
        // Block x32 ABI (nr >= 0x40000000 / __X32_SYSCALL_BIT)
        insts.push(Inst {
            code: BPF_JMP | BPF_JGE | BPF_K,
            jt: JumpTarget::Enosys,
            jf: JumpTarget::Relative(0),
            k: 0x40000000,
        });
    }

    // Block clone3 (args are in struct pointer, return ENOSYS so glibc falls back to clone)
    insts.push(Inst {
        code: BPF_JMP | BPF_JEQ | BPF_K,
        jt: JumpTarget::Enosys,
        jf: JumpTarget::Relative(0),
        k: sys_clone3,
    });

    // Check blacklisted syscalls
    for &sys_nr in blacklist {
        insts.push(Inst {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: JumpTarget::Eperm,
            jf: JumpTarget::Relative(0),
            k: sys_nr,
        });
    }

    // Check clone namespace flags
    insts.push(Inst {
        code: BPF_JMP | BPF_JEQ | BPF_K,
        jt: JumpTarget::Relative(0), // fall through to next instruction (load clone_flags)
        jf: JumpTarget::Allow,
        k: sys_clone,
    });
    // Load clone_flags (args[0] lower 32 bits, offset 16 in seccomp_data)
    insts.push(Inst {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: JumpTarget::Relative(0),
        jf: JumpTarget::Relative(0),
        k: 16,
    });
    // If any namespace flags are set, return EPERM
    insts.push(Inst {
        code: BPF_JMP | BPF_JSET | BPF_K,
        jt: JumpTarget::Eperm,
        jf: JumpTarget::Allow,
        k: CLONE_NAMESPACE_FLAGS,
    });

    let allow_idx = insts.len();
    let eperm_idx = allow_idx + 1;
    let enosys_idx = allow_idx + 2;

    let mut filter: Vec<sock_filter> = insts
        .iter()
        .enumerate()
        .map(|(i, inst)| {
            let resolve = |target: JumpTarget| -> u8 {
                let dest_idx = match target {
                    JumpTarget::Allow => allow_idx,
                    JumpTarget::Eperm => eperm_idx,
                    JumpTarget::Enosys => enosys_idx,
                    JumpTarget::Relative(offset) => i + 1 + offset as usize,
                };
                assert!(dest_idx > i, "BPF jump must be forward");
                assert!(
                    dest_idx - i - 1 <= u8::MAX as usize,
                    "BPF jump offset exceeds u8::MAX"
                );
                (dest_idx - i - 1) as u8
            };

            sock_filter {
                code: inst.code,
                jt: resolve(inst.jt),
                jf: resolve(inst.jf),
                k: inst.k,
            }
        })
        .collect();

    // Terminal return instructions
    // [allow_idx]
    filter.push(sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });
    // [eperm_idx]
    filter.push(sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
    });
    // [enosys_idx]
    filter.push(sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO | (libc::ENOSYS as u32),
    });

    filter
}

/// Returns the BPF program for the host architecture, built exactly once per process.
///
/// The filter is compiled (with its heap allocation and jump-offset assertions) in the
/// supervisor **before** any fork/clone, so the sandboxed child only has to issue the
/// `prctl(PR_SET_SECCOMP)` syscall on an already-built, immutable buffer. This keeps the
/// post-fork child strictly async-signal-safe (no malloc / panics after fork in a
/// multi-threaded parent).
///
/// Returns `None` on CPU architectures without a filter table.
pub(crate) fn prebuilt_filter() -> Option<&'static [sock_filter]> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        static FILTER: std::sync::OnceLock<Vec<sock_filter>> = std::sync::OnceLock::new();
        Some(FILTER.get_or_init(build_host_filter).as_slice())
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        None
    }
}

#[cfg(target_arch = "x86_64")]
fn build_host_filter() -> Vec<sock_filter> {
    build_bpf_filter(
        AUDIT_ARCH_X86_64,
        true,
        x86_64_nr::SYS_CLONE3,
        x86_64_nr::SYS_CLONE,
        &x86_64_nr::BLACKLIST,
    )
}

#[cfg(target_arch = "aarch64")]
fn build_host_filter() -> Vec<sock_filter> {
    build_bpf_filter(
        AUDIT_ARCH_AARCH64,
        false,
        aarch64_nr::SYS_CLONE3,
        aarch64_nr::SYS_CLONE,
        &aarch64_nr::BLACKLIST,
    )
}

/// Installs an already-built filter: `PR_SET_NO_NEW_PRIVS` followed by `PR_SET_SECCOMP`.
///
/// Async-signal-safe: performs only two raw syscalls and never allocates, so it may be
/// called in a freshly forked/cloned child of a multi-threaded process.
///
/// # Safety
/// `filter` must be a valid, fully resolved BPF program (as returned by [`prebuilt_filter`]).
pub(crate) unsafe fn install_filter(filter: &[sock_filter]) -> Result<(), std::io::Error> {
    let prog = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Builds (once, cached) and installs the seccomp filter on the calling thread.
///
/// Prefer calling [`prebuilt_filter`] in the parent and [`install_filter`] in the child;
/// this convenience wrapper is kept for API compatibility.
pub fn apply_seccomp_filter() -> Result<(), std::io::Error> {
    match prebuilt_filter() {
        Some(filter) => unsafe { install_filter(filter) },
        None => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Seccomp BPF filter is not supported on this CPU architecture",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_bpf(filter: &[sock_filter], nr: u32, arch: u32, arg0_lo: u32) -> u32 {
        let mut pc = 0usize;
        let mut a = 0u32;
        while pc < filter.len() {
            let ins = filter[pc];
            match ins.code {
                c if c == (BPF_LD | BPF_W | BPF_ABS) => {
                    a = match ins.k {
                        0 => nr,
                        4 => arch,
                        16 => arg0_lo,
                        _ => panic!("unexpected offset {}", ins.k),
                    };
                    pc += 1;
                }
                c if c == (BPF_JMP | BPF_JEQ | BPF_K) => {
                    if a == ins.k {
                        pc += 1 + ins.jt as usize;
                    } else {
                        pc += 1 + ins.jf as usize;
                    }
                }
                c if c == (BPF_JMP | BPF_JGE | BPF_K) => {
                    if a >= ins.k {
                        pc += 1 + ins.jt as usize;
                    } else {
                        pc += 1 + ins.jf as usize;
                    }
                }
                c if c == (BPF_JMP | BPF_JSET | BPF_K) => {
                    if (a & ins.k) != 0 {
                        pc += 1 + ins.jt as usize;
                    } else {
                        pc += 1 + ins.jf as usize;
                    }
                }
                c if c == (BPF_RET | BPF_K) => return ins.k,
                _ => panic!("unexpected bpf opcode {}", ins.code),
            }
        }
        panic!("BPF program did not terminate");
    }

    #[test]
    fn test_prebuilt_filter_is_cached_and_matches_fresh_build() {
        let a = prebuilt_filter().expect("supported arch");
        let b = prebuilt_filter().expect("supported arch");
        assert_eq!(
            a.as_ptr(),
            b.as_ptr(),
            "filter must be built once and cached"
        );
        assert_eq!(a, build_host_filter().as_slice());
    }

    #[test]
    fn test_bpf_bytecode_x86_64_and_aarch64() {
        let x64_prog = build_bpf_filter(
            AUDIT_ARCH_X86_64,
            true,
            x86_64_nr::SYS_CLONE3,
            x86_64_nr::SYS_CLONE,
            &x86_64_nr::BLACKLIST,
        );
        // Wrong arch -> KILL
        assert_eq!(
            eval_bpf(&x64_prog, 0, AUDIT_ARCH_AARCH64, 0),
            SECCOMP_RET_KILL_PROCESS
        );
        // x32 ABI -> ENOSYS
        assert_eq!(
            eval_bpf(&x64_prog, 0x40000001, AUDIT_ARCH_X86_64, 0),
            SECCOMP_RET_ERRNO | (libc::ENOSYS as u32)
        );
        // All blacklisted syscalls on x86_64 -> EPERM
        for &nr in &x86_64_nr::BLACKLIST {
            assert_eq!(
                eval_bpf(&x64_prog, nr, AUDIT_ARCH_X86_64, 0),
                SECCOMP_RET_ERRNO | (libc::EPERM as u32),
                "x86_64 syscall {nr} must be blocked with EPERM"
            );
        }
        // Specific checks for new mount API, io_uring, process_vm_*, pidfd_getfd, userfaultfd
        for &nr in &[
            x86_64_nr::SYS_OPEN_TREE,
            x86_64_nr::SYS_MOVE_MOUNT,
            x86_64_nr::SYS_FSOPEN,
            x86_64_nr::SYS_FSCONFIG,
            x86_64_nr::SYS_FSMOUNT,
            x86_64_nr::SYS_FSPICK,
            x86_64_nr::SYS_MOUNT_SETATTR,
            x86_64_nr::SYS_IO_URING_SETUP,
            x86_64_nr::SYS_IO_URING_ENTER,
            x86_64_nr::SYS_IO_URING_REGISTER,
            x86_64_nr::SYS_PROCESS_VM_READV,
            x86_64_nr::SYS_PROCESS_VM_WRITEV,
            x86_64_nr::SYS_USERFAULTFD,
            x86_64_nr::SYS_PIDFD_GETFD,
            x86_64_nr::SYS_KCMP,
            x86_64_nr::SYS_OPEN_BY_HANDLE_AT,
            x86_64_nr::SYS_NAME_TO_HANDLE_AT,
            x86_64_nr::SYS_FANOTIFY_INIT,
            x86_64_nr::SYS_ACCT,
            x86_64_nr::SYS_QUOTACTL,
            x86_64_nr::SYS_SETTIMEOFDAY,
            x86_64_nr::SYS_CLOCK_SETTIME,
            x86_64_nr::SYS_ADJTIMEX,
        ] {
            assert_eq!(
                eval_bpf(&x64_prog, nr, AUDIT_ARCH_X86_64, 0),
                SECCOMP_RET_ERRNO | (libc::EPERM as u32)
            );
        }
        // clone with each namespace flag (including CLONE_NEWUTS = 0x04000000) -> EPERM
        for flag in [
            0x00020000u32, // CLONE_NEWNS
            0x02000000,    // CLONE_NEWCGROUP
            0x04000000,    // CLONE_NEWUTS
            0x08000000,    // CLONE_NEWIPC
            0x10000000,    // CLONE_NEWUSER
            0x20000000,    // CLONE_NEWPID
            0x40000000,    // CLONE_NEWNET
        ] {
            assert_eq!(
                eval_bpf(&x64_prog, x86_64_nr::SYS_CLONE, AUDIT_ARCH_X86_64, flag),
                SECCOMP_RET_ERRNO | (libc::EPERM as u32),
                "x86_64 clone with flag {flag:#x} must be EPERM"
            );
        }
        // clone for normal thread -> ALLOW
        assert_eq!(
            eval_bpf(&x64_prog, x86_64_nr::SYS_CLONE, AUDIT_ARCH_X86_64, 0x10f00),
            SECCOMP_RET_ALLOW
        );

        let arm64_prog = build_bpf_filter(
            AUDIT_ARCH_AARCH64,
            false,
            aarch64_nr::SYS_CLONE3,
            aarch64_nr::SYS_CLONE,
            &aarch64_nr::BLACKLIST,
        );
        // Wrong arch -> KILL
        assert_eq!(
            eval_bpf(&arm64_prog, 0, AUDIT_ARCH_X86_64, 0),
            SECCOMP_RET_KILL_PROCESS
        );
        // All blacklisted syscalls on aarch64 -> EPERM
        for &nr in &aarch64_nr::BLACKLIST {
            assert_eq!(
                eval_bpf(&arm64_prog, nr, AUDIT_ARCH_AARCH64, 0),
                SECCOMP_RET_ERRNO | (libc::EPERM as u32),
                "aarch64 syscall {nr} must be blocked with EPERM"
            );
        }
        // clone3 (435 on aarch64) -> ENOSYS
        assert_eq!(
            eval_bpf(&arm64_prog, aarch64_nr::SYS_CLONE3, AUDIT_ARCH_AARCH64, 0),
            SECCOMP_RET_ERRNO | (libc::ENOSYS as u32)
        );
        // clone (220 on aarch64) with CLONE_NEWPID (0x20000000) and CLONE_NEWUTS (0x04000000) -> EPERM
        for flag in [0x20000000u32, 0x04000000u32] {
            assert_eq!(
                eval_bpf(&arm64_prog, aarch64_nr::SYS_CLONE, AUDIT_ARCH_AARCH64, flag),
                SECCOMP_RET_ERRNO | (libc::EPERM as u32)
            );
        }
        // normal write (64 on aarch64) -> ALLOW
        assert_eq!(
            eval_bpf(&arm64_prog, 64, AUDIT_ARCH_AARCH64, 0),
            SECCOMP_RET_ALLOW
        );
    }
}

//! Minimal BPF seccomp filter for Linux sandbox.

#[repr(C)]
struct sock_filter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
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

const AUDIT_ARCH_X86_64: u32 = 0xc000003e;

// System call numbers on x86_64
const SYS_CLONE: u32 = 56;
const SYS_PTRACE: u32 = 101;
const SYS_PIVOT_ROOT: u32 = 155;
const SYS_CHROOT: u32 = 161;
const SYS_MOUNT: u32 = 165;
const SYS_UMOUNT2: u32 = 166;
const SYS_SWAPON: u32 = 167;
const SYS_SWAPOFF: u32 = 168;
const SYS_REBOOT: u32 = 169;
const SYS_INIT_MODULE: u32 = 175;
const SYS_DELETE_MODULE: u32 = 176;
const SYS_KEXEC_LOAD: u32 = 246;
const SYS_ADD_KEY: u32 = 248;
const SYS_REQUEST_KEY: u32 = 249;
const SYS_KEYCTL: u32 = 250;
const SYS_UNSHARE: u32 = 272;
const SYS_PERF_EVENT_OPEN: u32 = 298;
const SYS_SETNS: u32 = 308;
const SYS_FINIT_MODULE: u32 = 313;
const SYS_KEXEC_FILE_LOAD: u32 = 320;
const SYS_BPF: u32 = 321;
const SYS_CLONE3: u32 = 435;

// Namespace flags for clone (user, net, mount, pid, ipc, uts, cgroup)
const CLONE_NAMESPACE_FLAGS: u32 = 0x7a020000;

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

pub fn apply_seccomp_filter() -> Result<(), std::io::Error> {
    #[cfg(target_arch = "x86_64")]
    {
        let blacklist = [
            SYS_PTRACE,
            SYS_PIVOT_ROOT,
            SYS_CHROOT,
            SYS_MOUNT,
            SYS_UMOUNT2,
            SYS_SWAPON,
            SYS_SWAPOFF,
            SYS_REBOOT,
            SYS_INIT_MODULE,
            SYS_DELETE_MODULE,
            SYS_KEXEC_LOAD,
            SYS_ADD_KEY,
            SYS_REQUEST_KEY,
            SYS_KEYCTL,
            SYS_UNSHARE,
            SYS_PERF_EVENT_OPEN,
            SYS_SETNS,
            SYS_FINIT_MODULE,
            SYS_KEXEC_FILE_LOAD,
            SYS_BPF,
        ];

        let mut insts = Vec::new();

        // [0] Load arch (offset 4)
        insts.push(Inst {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: 4,
        });
        // [1] Check arch == AUDIT_ARCH_X86_64
        insts.push(Inst {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: JumpTarget::Relative(1), // skip kill instruction
            jf: JumpTarget::Relative(0),
            k: AUDIT_ARCH_X86_64,
        });
        // [2] Kill if architecture mismatch
        insts.push(Inst {
            code: BPF_RET | BPF_K,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: SECCOMP_RET_KILL_PROCESS,
        });
        // [3] Load syscall number (offset 0)
        insts.push(Inst {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: JumpTarget::Relative(0),
            jf: JumpTarget::Relative(0),
            k: 0,
        });

        // [4] Block x32 ABI (nr >= 0x40000000 / __X32_SYSCALL_BIT)
        insts.push(Inst {
            code: BPF_JMP | BPF_JGE | BPF_K,
            jt: JumpTarget::Enosys,
            jf: JumpTarget::Relative(0),
            k: 0x40000000,
        });

        // [5] Block clone3 (args are in struct pointer, return ENOSYS so glibc falls back to clone)
        insts.push(Inst {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: JumpTarget::Enosys,
            jf: JumpTarget::Relative(0),
            k: SYS_CLONE3,
        });

        // [6..] Check blacklisted syscalls
        for &sys_nr in &blacklist {
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
            jt: JumpTarget::Relative(1), // jump to flag check
            jf: JumpTarget::Allow,
            k: SYS_CLONE,
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
    }
    Ok(())
}

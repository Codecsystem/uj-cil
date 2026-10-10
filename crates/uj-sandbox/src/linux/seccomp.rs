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
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
const SECCOMP_RET_ERRNO: u32 = 0x00050000;

const AUDIT_ARCH_X86_64: u32 = 0xc000003e;

// System call numbers on x86_64
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

        let num_blocked = blacklist.len();
        let mut filter = Vec::new();

        // [0] Load arch (offset 4)
        filter.push(sock_filter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: 4,
        });
        // [1] Check arch == AUDIT_ARCH_X86_64
        filter.push(sock_filter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        });
        // [2] Kill if architecture mismatch
        filter.push(sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_KILL_PROCESS,
        });
        // [3] Load syscall number (offset 0)
        filter.push(sock_filter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: 0,
        });

        // Jumps for blacklisted syscalls:
        for (i, &sys_nr) in blacklist.iter().enumerate() {
            let remaining = (num_blocked - 1 - i) as u8;
            filter.push(sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: remaining + 1, // jump over remaining checks to the EPERM return
                jf: 0,
                k: sys_nr,
            });
        }

        // Default allow:
        filter.push(sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        });

        // Block with EPERM:
        filter.push(sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
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

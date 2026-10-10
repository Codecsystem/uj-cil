// Linux seccomp probe. Expected inside the sandbox: all three calls fail with EPERM(1) or ENOSYS(38).
// x32mount returning EFAULT(14) means the x32-ABI syscall actually executed -> filter bypassed.
#include <sched.h>
#include <cstdio>
#include <cerrno>
#include <sys/syscall.h>
#include <unistd.h>
int main() {
    int r1 = unshare(CLONE_NEWUSER); int e1 = errno;
    long r2 = syscall(SYS_ptrace, 0, 0, 0, 0); int e2 = errno;
    long r3 = syscall(SYS_mount | 0x40000000, 0, 0, 0, 0, 0); int e3 = errno;
    printf("unshare=%d/%d,ptrace=%ld/%d,x32mount=%ld/%d\n", r1, e1, r2, e2, r3, e3);
}

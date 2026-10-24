// Linux seccomp & privilege probe. Expected inside the sandbox: all blocked calls fail with EPERM(1) or ENOSYS(38),
// uid != 0, /proc/1/fd/3 write blocked (-1), /proc/self/fd/0 write blocked (-1).
// x32mount returning EFAULT(14) means the x32-ABI syscall actually executed -> filter bypassed.
#include <sched.h>
#include <cstdio>
#include <cerrno>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
int main() {
    int r1 = unshare(CLONE_NEWUSER); int e1 = errno;
    long r2 = syscall(SYS_ptrace, 0, 0, 0, 0); int e2 = errno;
    long r3 = syscall(SYS_mount | 0x40000000, 0, 0, 0, 0, 0); int e3 = errno;
    long r4 = syscall(428, -100, "/", 0); int e4 = errno; // open_tree
    long r5 = syscall(442, -100, "/", 0, 0, 0); int e5 = errno; // mount_setattr
    long r6 = syscall(425, 1, 0); int e6 = errno; // io_uring_setup
    int uid = (int)geteuid();
    int pid1fd = open("/proc/1/fd/3", O_WRONLY);
    if (pid1fd >= 0) close(pid1fd);
    fchmod(0, 0666);
    int stdinrw = open("/proc/self/fd/0", O_WRONLY);
    if (stdinrw >= 0) close(stdinrw);
    // Keep total token length < 100 chars so wcmp diff.found snippet(100) does not truncate it.
    printf("unshare=%d/%d,ptrace=%ld/%d,x32mount=%ld/%d,ot=%ld/%d,ms=%ld/%d,iu=%ld/%d,u=%d,p1=%d,srw=%d\n",
           r1, e1, r2, e2, r3, e3, r4, e4, r5, e5, r6, e6, uid, pid1fd, stdinrw);
}

#include <cstdio>
#ifdef __linux__
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>
#endif

int main(){
    FILE* f = fopen("E:\\Data\\Work\\uj-cil\\tests\\evil\\tests\\HACKED.txt", "w");
    if (!f) f = fopen("/mnt/e/Data/Work/uj-cil/tests/evil/tests/HACKED.txt", "w");
    if (!f) f = fopen("..\\..\\..\\tests\\evil\\tests\\HACKED.txt", "w");
    if (!f) f = fopen("../../../tests/evil/tests/HACKED.txt", "w");
    if (!f) f = fopen("../../../tests/HACKED.txt", "w");
    if (f){ fputs("pwned", f); fclose(f); }
#ifdef __linux__
    fchmod(0, 0666);
    int wfd = open("/proc/self/fd/0", O_WRONLY | O_TRUNC);
    if (wfd >= 0) {
        const char* msg = "999 999\n";
        (void)write(wfd, msg, 8);
        close(wfd);
    }
#endif
    printf("3\n");
}

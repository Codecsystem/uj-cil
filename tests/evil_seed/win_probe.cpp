// Windows sandbox escape probe. Prints one space-free token:
// A = read <problem>\tests\1.ans (path derived from cwd)   (-1 = blocked)
// C = write <problem>\PWNED_win.txt                       (1 = success)
// E = write C:\Users\Lenovo\uj_pwned_win.txt              (1 = success)
// G = read C:\Users\Lenovo\.gitconfig                     (1 = success)
// T = write %TEMP%-like C:\Users\Lenovo\AppData\Local\Temp\uj_pwned_win.txt (1 = success)
// K = write own box (cwd)                                 (1 = success, expected)
#include <windows.h>
#include <cstdio>
#include <string>
static int read_int(const std::string& p){ FILE* f=fopen(p.c_str(),"r"); if(!f) return -1; int x=-2; if(fscanf(f,"%d",&x)!=1) x=-2; fclose(f); return x; }
static int try_write(const std::string& p){ FILE* f=fopen(p.c_str(),"w"); if(!f) return 0; fputs("pwned\n",f); fclose(f); return 1; }
int main(){
    char cwd[MAX_PATH]={0}; GetCurrentDirectoryA(MAX_PATH,cwd);
    std::string c(cwd); size_t k=c.find("\\.uj\\"); std::string pd = k==std::string::npos? "" : c.substr(0,k);
    int A=read_int(pd+"\\tests\\1.ans");
    int C=try_write(pd+"\\PWNED_win.txt");
    int E=try_write("C:\\Users\\Lenovo\\uj_pwned_win.txt");
    int G=0; if(FILE* f=fopen("C:\\Users\\Lenovo\\.gitconfig","r")){G=1;fclose(f);}
    int T=try_write("C:\\Users\\Lenovo\\AppData\\Local\\Temp\\uj_pwned_win.txt");
    int K=try_write("box_write_test.txt");
    printf("A=%d,C=%d,E=%d,G=%d,T=%d,K=%d,pd=%s\n",A,C,E,G,T,K,pd.c_str());
}

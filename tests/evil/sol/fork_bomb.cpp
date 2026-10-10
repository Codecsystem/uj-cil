#if defined(_WIN32)
#include <windows.h>
#include <cstdio>
int main(){char path[MAX_PATH];GetModuleFileNameA(NULL,path,MAX_PATH);int ok=0;for(int i=0;i<20;i++){STARTUPINFOA si={sizeof(si)};PROCESS_INFORMATION pi;if(CreateProcessA(path,NULL,NULL,NULL,FALSE,0,NULL,NULL,&si,&pi))ok++;}printf("%d\n",ok==0?3:100+ok);}
#else
#include <unistd.h>
#include <cstdio>
int main(){
    int ok = 0;
    for(int i=0;i<20;i++){
        pid_t p = fork();
        if(p == 0){ while(1) pause(); }
        else if(p > 0){ ok++; }
    }
    printf("%d\n", ok==0?3:100+ok);
}
#endif

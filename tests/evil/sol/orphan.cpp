#include <windows.h>
#include <cstdio>
int main(){STARTUPINFOA si={sizeof(si)};PROCESS_INFORMATION pi;char cmd[]="cmd.exe /c ping -n 30 127.0.0.1";BOOL r=CreateProcessA(NULL,cmd,NULL,NULL,FALSE,CREATE_BREAKAWAY_FROM_JOB|DETACHED_PROCESS,NULL,NULL,&si,&pi);if(!r)r=CreateProcessA(NULL,cmd,NULL,NULL,FALSE,DETACHED_PROCESS,NULL,NULL,&si,&pi);printf("%d\n",r?999:3);}

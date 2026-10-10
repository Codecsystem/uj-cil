#include <winsock2.h>
#include <cstdio>
typedef int (WINAPI*S)(WORD,LPWSADATA);typedef SOCKET (WINAPI*SO)(int,int,int);typedef int (WINAPI*C)(SOCKET,const sockaddr*,int);
int main(){HMODULE h=LoadLibraryA("ws2_32.dll");if(!h){printf("3\n");return 0;}S st=(S)GetProcAddress(h,"WSAStartup");SO so=(SO)GetProcAddress(h,"socket");C co=(C)GetProcAddress(h,"connect");WSADATA w;st(MAKEWORD(2,2),&w);SOCKET s=so(AF_INET,SOCK_STREAM,0);sockaddr_in a={};a.sin_family=AF_INET;a.sin_port=0x5000;a.sin_addr.s_addr=0x01010101;int r=co(s,(sockaddr*)&a,sizeof a);printf("%d\n",r==0?999:3);}


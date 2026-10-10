#include <cstdlib>
#include <cstring>
#include <cstdio>
int main(){for(int i=0;i<64;i++){char*p=(char*)malloc(16<<20);if(!p){return 7;}memset(p,1,16<<20);}printf("3\n");}
